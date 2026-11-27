# libusbdac.so finds these by name through JNI; R8 must not rename or remove them.
-keep class dev.usbdac.internal.Native { *; }
-keep class dev.usbdac.internal.AAudioNative { *; }
-keep class dev.usbdac.internal.ConvertNative { *; }
-keep class dev.usbdac.internal.DsdPcmNative { *; }
-keep interface dev.usbdac.internal.NativeListener { *; }
-keepclassmembers class * implements dev.usbdac.internal.NativeListener {
    public void onEvent(int, long, double, java.lang.String);
}
