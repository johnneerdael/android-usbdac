/* Fixture-only USBFS launcher and endpoint-zero control helper (Linux). */
#define _GNU_SOURCE
#include <errno.h>
#include <fcntl.h>
#include <linux/usb/ch9.h>
#include <linux/usbdevice_fs.h>
#include <stdio.h>
#include <stdlib.h>
#include <string.h>
#include <sys/ioctl.h>
#include <unistd.h>

static void die(const char *what) { perror(what); exit(1); }
static int control(int fd, unsigned char type, unsigned short value, void *data, unsigned short length) {
    struct usbdevfs_ctrltransfer x = { .bRequestType = type, .bRequest = 0x5a,
        .wValue = value, .wIndex = 0, .wLength = length, .timeout = 3000, .data = data };
    int n = ioctl(fd, USBDEVFS_CONTROL, &x);
    if (n < 0) die("fixture control");
    if (n != length) { fprintf(stderr, "short fixture control: %d/%u\n", n, length); exit(1); }
    return n;
}
static int open_fixture(const char *path) {
    unsigned bus, dev; char extra;
    if (sscanf(path, "/dev/bus/usb/%3u/%3u%c", &bus, &dev, &extra) != 2 ||
        strlen(path) != strlen("/dev/bus/usb/000/000") || bus == 0 || dev == 0) {
        fprintf(stderr, "expected /dev/bus/usb/BBB/DDD\n"); exit(1);
    }
    int fd = open(path, O_RDWR | O_CLOEXEC);
    if (fd < 0) die("open USBFS fixture");
    unsigned char desc[18];
    struct usbdevfs_ctrltransfer req = { .bRequestType = USB_DIR_IN | USB_TYPE_STANDARD | USB_RECIP_DEVICE,
        .bRequest = USB_REQ_GET_DESCRIPTOR, .wValue = USB_DT_DEVICE << 8,
        .wLength = sizeof desc, .timeout = 3000, .data = desc };
    int n = ioctl(fd, USBDEVFS_CONTROL, &req);
    if (n != 18 || desc[0] != 18 || desc[1] != USB_DT_DEVICE ||
        desc[8] != 0x09 || desc[9] != 0x12 || desc[10] != 0xca || desc[11] != 0xda) {
        fprintf(stderr, "USB device is not fixture 1209:daca (descriptor length %d)\n", n); exit(1);
    }
    return fd;
}
int main(int argc, char **argv) {
    if (argc == 4 && strcmp(argv[1], "--control") == 0) {
        int fd = open_fixture(argv[2]);
        if (!strcmp(argv[3], "arm")) control(fd, 0x40, 1, NULL, 0);
        else if (!strcmp(argv[3], "release")) control(fd, 0x40, 2, NULL, 0);
        else if (!strcmp(argv[3], "status")) { unsigned char status = 0; control(fd, 0xc0, 0, &status, 1); printf("%u\n", status); }
        else { fprintf(stderr, "unknown fixture command\n"); return 2; }
        close(fd); return 0;
    }
    if (argc < 3) { fprintf(stderr, "usage: %s USBFS_PATH JAVA [ARGS...]\n", argv[0]); return 2; }
    int fd = open_fixture(argv[1]);
    for (unsigned iface = 0; iface < 2; ++iface)
        if (ioctl(fd, USBDEVFS_CLAIMINTERFACE, &iface) < 0) die("claim fixture interface");
    if (fcntl(fd, F_SETFD, 0) < 0) die("clear FD_CLOEXEC");
    char fd_text[32]; snprintf(fd_text, sizeof fd_text, "%d", fd);
    char *self = realpath(argv[0], NULL);
    if (!self || setenv("USBDAC_FIXTURE_FD", fd_text, 1) || setenv("USBDAC_FIXTURE_PATH", argv[1], 1) || setenv("USBDAC_FIXTURE_LAUNCHER", self, 1)) die("set fixture environment");
    execvp(argv[2], &argv[2]); die("exec JVM");
}
