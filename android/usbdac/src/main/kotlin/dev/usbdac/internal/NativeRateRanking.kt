package dev.usbdac.internal

/** Pure native ranking, shared with Rust USB negotiation. Library loading belongs to the caller. */
internal object NativeRateRanking {
    @JvmStatic external fun pcmAdmission(valuesJson: String): String
    @JvmStatic external fun rank(requested: Int, ratesJson: String): String

    fun rankList(requested: Int, rates: List<Int>): List<Int> {
        val result = org.json.JSONArray(rank(requested, org.json.JSONArray(rates).toString()))
        return (0 until result.length()).map { result.getInt(it) }
    }
}
