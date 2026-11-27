import dev.usbdac.internal.Native;
import java.nio.ByteBuffer;
import java.nio.charset.StandardCharsets;
import java.util.concurrent.TimeUnit;
import java.util.regex.Matcher;
import java.util.regex.Pattern;

/** Exercises the production JNI API against the disposable 1209:daca fixture. */
public final class JniRetirementProbe {
    public static final class Listener {
        public void onEvent(int type, long a, double b, String message) {
            System.out.printf("event type=%d a=%d b=%s message=%s%n", type, a, b, message);
        }
    }
    private static String env(String name) {
        String value = System.getenv(name);
        if (value == null || value.isEmpty()) throw new IllegalStateException("missing " + name);
        return value;
    }
    private static int control(String command) throws Exception {
        ProcessBuilder builder = command.equals("arm")
                ? new ProcessBuilder(env("USBDAC_FIXTURE_LAUNCHER"), "--control", env("USBDAC_FIXTURE_PATH"), command)
                : new ProcessBuilder("/fixture/usbip_uac", "--control", command, "--port", env("USBDAC_USBIP_PORT"));
        Process process = builder.redirectError(ProcessBuilder.Redirect.INHERIT).start();
        if (!process.waitFor(5, TimeUnit.SECONDS)) { process.destroyForcibly(); throw new IllegalStateException("fixture control timed out: " + command); }
        byte[] out = process.getInputStream().readAllBytes();
        if (process.exitValue() != 0) throw new IllegalStateException("fixture control failed: " + command);
        return command.equals("status") ? Integer.parseInt(new String(out, StandardCharsets.US_ASCII).trim()) : 0;
    }
    private static void require(boolean condition, String message) {
        if (!condition) throw new IllegalStateException(message);
    }
    private static void waitStatus(int mask) throws Exception {
        long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
        int status;
        do {
            status = control("status");
            if ((status & mask) == mask) return;
            Thread.sleep(50);
        } while (System.nanoTime() < deadline);
        throw new IllegalStateException("fixture status " + status + " missing " + mask);
    }
    private static long formatId(String json) {
        Matcher match = Pattern.compile("\\\"format_id\\\"\\s*:\\s*(\\d+)").matcher(json);
        require(match.find(), "negotiation lacks format_id: " + json);
        return Long.parseLong(match.group(1));
    }
    private static boolean retirementError(IllegalStateException error) {
        String message = String.valueOf(error.getMessage()).toLowerCase();
        return message.contains("retir") || message.contains("timed out") || message.contains("timeout");
    }
    public static void main(String[] args) throws Exception {
        int fd = Integer.parseInt(env("USBDAC_FIXTURE_FD"));
        String device = "{\"vendor_id\":4617,\"product_id\":56010,\"product_name\":\"USB lifecycle fixture\",\"device_name\":\"" + env("USBDAC_FIXTURE_PATH") + "\",\"claimed_interfaces\":[0,1]}";
        String config = "{\"require_hardware_volume\":false,\"require_verified_rate\":true,\"startup_volume\":\"keep\",\"hand_back\":\"keep\"}";
        String request = "{\"encoding\":\"pcm\",\"sample_rate\":48000,\"channels\":2,\"bit_depth\":16,\"rate_policy\":\"exact\"}";
        long dac = 0, stream = 0, token = 0;
        boolean armed = false;
        try {
            dac = Native.dacOpen(fd, device, config);
            String negotiated = Native.dacNegotiate(dac, request);
            System.out.println("negotiated=" + negotiated);
            stream = Native.streamOpen(dac, formatId(negotiated), new Listener());
            ByteBuffer pcm = ByteBuffer.allocateDirect(19200);
            for (int i = 0; i < pcm.capacity(); i++) pcm.put(i, (byte) (i * 17));
            int written = Native.streamWrite(stream, pcm, 0, pcm.capacity());
            require(written > 0, "no PCM accepted");
            Native.streamPlay(stream);
            long deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(10);
            while (Native.streamPlayedFrames(stream) == 0 && System.nanoTime() < deadline) Thread.sleep(50);
            require(Native.streamPlayedFrames(stream) > 0, "no PCM played");
            System.out.println("PCM_PLAYED frames=" + Native.streamPlayedFrames(stream));
            token = Native.dacRetirementToken(dac);
            require(token != 0, "invalid retirement token");
            control("arm"); armed = true;
            waitStatus(2);
            Native.dacSetCloseHandBack(dac, false, false);
            boolean closeFailed = false;
            try { Native.streamClose(stream); } catch (IllegalStateException e) {
                closeFailed = retirementError(e); System.out.println("streamClose=" + e.getMessage());
            } finally { stream = 0; }
            try { Native.dacClose(dac); } catch (IllegalStateException e) {
                closeFailed |= retirementError(e); System.out.println("dacClose=" + e.getMessage());
            } finally { dac = 0; }
            System.out.println("JNI_NATIVE_CLOSE_RETURNED");
            require(closeFailed, "close did not report timeout/retiring");
            System.out.println("JNI_OOB_STATUS_BEGIN");
            waitStatus(0x16); // submit held, unlink held, alternate setting 1 active
            require(!Native.dacAwaitRetirementToken(token), "owner retired while fixture holds unlink");
            boolean releaseRefused = false;
            try { Native.dacReleaseRetirementToken(token); } catch (IllegalStateException e) {
                releaseRefused = retirementError(e);
                System.out.println("tokenRelease=" + e.getMessage());
            }
            require(releaseRefused, "pending owner token release was not refused");
            require(!Native.dacAwaitRetirementToken(token), "token lost or owner retired before release");
            System.out.println("JNI_RETIREMENT_PENDING");
            System.out.println("JNI_OOB_RELEASE_BEGIN");
            control("release"); armed = false;
            deadline = System.nanoTime() + TimeUnit.SECONDS.toNanos(15);
            boolean retired = false;
            do { retired = Native.dacAwaitRetirementToken(token); } while (!retired && System.nanoTime() < deadline);
            require(retired, "owner did not retire after release");
            Native.dacReleaseRetirementToken(token); token = 0;
            System.out.println("JNI_RETIREMENT_PROBE_PASS");
        } finally {
            if (armed) control("release");
            if (stream != 0) try { Native.streamClose(stream); } catch (IllegalStateException ignored) { }
            if (dac != 0) try { Native.dacClose(dac); } catch (IllegalStateException ignored) { }
            if (token != 0 && Native.dacAwaitRetirementToken(token)) Native.dacReleaseRetirementToken(token);
        }
    }
}
