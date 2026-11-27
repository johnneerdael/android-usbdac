package dev.usbdac.validator;
interface IUsbGrantService {
    String grant(String deviceName, int deviceId, int vendorId, int productId) = 1;
    void destroy() = 16777114;
}
