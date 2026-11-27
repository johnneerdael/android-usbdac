package dev.usbdac.validator.ui

import android.content.Context
import androidx.core.content.edit
import androidx.compose.animation.core.CubicBezierEasing
import androidx.compose.foundation.isSystemInDarkTheme
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.material3.ColorScheme
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.Shapes
import androidx.compose.material3.Typography
import androidx.compose.material3.darkColorScheme
import androidx.compose.material3.lightColorScheme
import androidx.compose.runtime.Composable
import androidx.compose.runtime.CompositionLocalProvider
import androidx.compose.runtime.Immutable
import androidx.compose.runtime.staticCompositionLocalOf
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.text.ExperimentalTextApi
import androidx.compose.ui.text.TextStyle
import androidx.compose.ui.text.font.Font
import androidx.compose.ui.text.font.FontFamily
import androidx.compose.ui.text.font.FontVariation
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.usbdac.validator.R

/** Semantic brand roles from branding/tokens/usbdac.json. Material roles are mapped from these. */
@Immutable
data class BrandColors(
    val dark: Boolean,
    val canvas: Color,
    val surface: Color,
    val raised: Color,
    val text: Color,
    val muted: Color,
    val primary: Color,
    val onPrimary: Color,
    val primarySoft: Color,
    val secondary: Color,
    val border: Color,
    val controlBorder: Color,
    val success: Color,
    val successSoft: Color,
    val warning: Color,
    val warningSoft: Color,
    val danger: Color,
    val dangerSoft: Color,
    val focus: Color,
)

val DarkBrand = BrandColors(
    dark = true,
    canvas = Color(0xFF11151C), surface = Color(0xFF1A212C), raised = Color(0xFF252F3D),
    text = Color(0xFFF3F5F7), muted = Color(0xFFB2C0D2),
    primary = Color(0xFF78B5FF), onPrimary = Color(0xFF101E32), primarySoft = Color(0xFF213753),
    secondary = Color(0xFFCADCF3), border = Color(0xFF415169), controlBorder = Color(0xFF7386A1),
    success = Color(0xFF87DFC2), successSoft = Color(0xFF203D34),
    warning = Color(0xFFF1CA83), warningSoft = Color(0xFF413723),
    danger = Color(0xFFFFABB8), dangerSoft = Color(0xFF442C36),
    focus = Color(0xFFA3CDFF),
)

val LightBrand = BrandColors(
    dark = false,
    canvas = Color(0xFFF2F5FA), surface = Color(0xFFFFFFFF), raised = Color(0xFFE7EDF5),
    text = Color(0xFF152237), muted = Color(0xFF4E6078),
    primary = Color(0xFF2367C9), onPrimary = Color(0xFFFFFFFF), primarySoft = Color(0xFFE1EDFF),
    secondary = Color(0xFF344E70), border = Color(0xFFACB9CA), controlBorder = Color(0xFF6C809B),
    success = Color(0xFF17634D), successSoft = Color(0xFFDDF4EA),
    warning = Color(0xFF76500B), warningSoft = Color(0xFFFFF0D2),
    danger = Color(0xFFA02945), dangerSoft = Color(0xFFFCE5EC),
    focus = Color(0xFF1358B7),
)

val LocalBrand = staticCompositionLocalOf { DarkBrand }

private fun BrandColors.toScheme(): ColorScheme {
    val base = if (dark) darkColorScheme() else lightColorScheme()
    return base.copy(
        primary = primary, onPrimary = onPrimary,
        primaryContainer = primarySoft, onPrimaryContainer = focus,
        inversePrimary = if (dark) LightBrand.primary else DarkBrand.primary,
        secondary = secondary, onSecondary = canvas,
        secondaryContainer = primarySoft, onSecondaryContainer = text,
        tertiary = success, onTertiary = canvas, tertiaryContainer = successSoft, onTertiaryContainer = success,
        background = canvas, onBackground = text,
        surface = surface, onSurface = text,
        surfaceVariant = raised, onSurfaceVariant = muted,
        surfaceTint = Color.Transparent,
        inverseSurface = text, inverseOnSurface = canvas,
        error = danger, onError = canvas, errorContainer = dangerSoft, onErrorContainer = danger,
        outline = controlBorder, outlineVariant = border,
        scrim = Color.Black,
        surfaceBright = raised, surfaceDim = canvas,
        surfaceContainerLowest = canvas, surfaceContainerLow = surface, surfaceContainer = surface,
        surfaceContainerHigh = raised, surfaceContainerHighest = raised,
    )
}

/** Manrope is a variable font; each weight is an instance of the same file. */
@OptIn(ExperimentalTextApi::class)
private fun manrope(weight: Int) = Font(
    R.font.manrope,
    weight = FontWeight(weight),
    variationSettings = FontVariation.Settings(FontVariation.weight(weight)),
)

val Manrope = FontFamily(manrope(400), manrope(500), manrope(600), manrope(700), manrope(750), manrope(800))
val PlexMono = FontFamily(Font(R.font.ibm_plex_mono_regular, FontWeight.Normal))

/** Technical text: device IDs, rates, counts and diagnostic output. */
val MonoStyle = TextStyle(fontFamily = PlexMono, fontWeight = FontWeight.Normal, fontSize = 12.5.sp, lineHeight = 19.sp)

private val UsbdacTypography = Typography().let { t ->
    fun TextStyle.m(weight: Int, letterSpacing: Float? = null) =
        copy(fontFamily = Manrope, fontWeight = FontWeight(weight), letterSpacing = letterSpacing?.sp ?: this.letterSpacing)
    Typography(
        displayLarge = t.displayLarge.m(750, -1f), displayMedium = t.displayMedium.m(750, -0.5f), displaySmall = t.displaySmall.m(750, -0.5f),
        headlineLarge = t.headlineLarge.m(750, -0.5f), headlineMedium = t.headlineMedium.m(750, -0.4f), headlineSmall = t.headlineSmall.m(700, -0.2f),
        titleLarge = t.titleLarge.m(700, -0.1f), titleMedium = t.titleMedium.m(700, 0f), titleSmall = t.titleSmall.m(650, 0f),
        bodyLarge = t.bodyLarge.m(400, 0f), bodyMedium = t.bodyMedium.m(400, 0f), bodySmall = t.bodySmall.m(400, 0f),
        labelLarge = t.labelLarge.m(650, 0f), labelMedium = t.labelMedium.m(600, 0.2f), labelSmall = t.labelSmall.m(600, 0.3f),
    )
}

private val UsbdacShapes = Shapes(
    extraSmall = RoundedCornerShape(8.dp), small = RoundedCornerShape(8.dp),
    medium = RoundedCornerShape(16.dp), large = RoundedCornerShape(24.dp), extraLarge = RoundedCornerShape(24.dp),
)

/** Motion tokens: feedback 120 ms, transition 180 ms, sheet 240 ms on cubic-bezier(.2,.8,.2,1). */
object Motion {
    val ease = CubicBezierEasing(0.2f, 0.8f, 0.2f, 1f)
    const val FEEDBACK = 120
    const val TRANSITION = 180
    const val SHEET = 240
}

enum class ThemeChoice(val label: String) { System("System"), Dark("Dark"), Light("Light") }

/** The appearance choice is stored per install; the app follows the system until the user picks. */
object ThemePreference {
    private const val FILE = "appearance"
    private const val KEY = "theme"
    fun load(context: Context): ThemeChoice =
        context.getSharedPreferences(FILE, Context.MODE_PRIVATE).getString(KEY, null)
            ?.let { name -> ThemeChoice.entries.firstOrNull { it.name == name } } ?: ThemeChoice.System
    fun store(context: Context, choice: ThemeChoice) {
        context.getSharedPreferences(FILE, Context.MODE_PRIVATE).edit { putString(KEY, choice.name) }
    }
}

@Composable
fun ThemeChoice.isDark(): Boolean = when (this) {
    ThemeChoice.System -> isSystemInDarkTheme()
    ThemeChoice.Dark -> true
    ThemeChoice.Light -> false
}

@Composable
fun UsbdacTheme(dark: Boolean, content: @Composable () -> Unit) {
    val brand = if (dark) DarkBrand else LightBrand
    CompositionLocalProvider(LocalBrand provides brand) {
        MaterialTheme(colorScheme = brand.toScheme(), typography = UsbdacTypography, shapes = UsbdacShapes, content = content)
    }
}
