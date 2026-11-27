package dev.usbdac.internal

import org.junit.Assert.*
import org.junit.Test

class UsbAttachmentOwnersTest {
    @Test fun ownersAreIndependentAndOldCloseCannotReleaseAReplacement() {
        val owners = UsbAttachmentOwners()
        val first = owners.acquire(1, "/dev/bus/usb/001/002")!!
        val second = owners.acquire(2, "/dev/bus/usb/001/003")!!
        assertNull(owners.acquire(1, "/dev/bus/usb/001/002"))
        first.close()
        val replacement = owners.acquire(1, "/dev/bus/usb/001/002")!!
        first.close()
        assertNull(owners.acquire(1, "/dev/bus/usb/001/002"))
        assertNull(owners.acquire(2, "/dev/bus/usb/001/003"))
        replacement.close()
        second.close()
        assertNotNull(owners.acquire(2, "/dev/bus/usb/001/003"))
    }
}
