package dev.usbdac.validator.ui

import androidx.compose.animation.AnimatedVisibility
import androidx.compose.animation.animateColorAsState
import androidx.compose.animation.animateContentSize
import androidx.compose.animation.core.tween
import androidx.compose.animation.expandVertically
import androidx.compose.animation.fadeIn
import androidx.compose.animation.fadeOut
import androidx.compose.animation.shrinkVertically
import androidx.compose.foundation.BorderStroke
import androidx.compose.foundation.Image
import androidx.compose.foundation.background
import androidx.compose.foundation.layout.Arrangement
import androidx.compose.foundation.layout.Box
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.ColumnScope
import androidx.compose.foundation.layout.PaddingValues
import androidx.compose.foundation.layout.Row
import androidx.compose.foundation.layout.Spacer
import androidx.compose.foundation.layout.WindowInsets
import androidx.compose.foundation.layout.consumeWindowInsets
import androidx.compose.foundation.layout.fillMaxSize
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.layout.imePadding
import androidx.compose.foundation.layout.navigationBars
import androidx.compose.foundation.layout.navigationBarsPadding
import androidx.compose.foundation.layout.padding
import androidx.compose.foundation.layout.size
import androidx.compose.foundation.layout.widthIn
import androidx.compose.foundation.layout.windowInsetsBottomHeight
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.selection.toggleable
import androidx.compose.foundation.shape.CircleShape
import androidx.compose.foundation.shape.RoundedCornerShape
import androidx.compose.foundation.text.selection.SelectionContainer
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.Button
import androidx.compose.material3.ButtonDefaults
import androidx.compose.material3.ExperimentalMaterial3Api
import androidx.compose.material3.FilledTonalButton
import androidx.compose.material3.HorizontalDivider
import androidx.compose.material3.Icon
import androidx.compose.material3.LinearProgressIndicator
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.OutlinedButton
import androidx.compose.material3.OutlinedTextField
import androidx.compose.material3.OutlinedTextFieldDefaults
import androidx.compose.material3.Scaffold
import androidx.compose.material3.SegmentedButton
import androidx.compose.material3.SegmentedButtonDefaults
import androidx.compose.material3.SingleChoiceSegmentedButtonRow
import androidx.compose.material3.SnackbarHost
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Surface
import androidx.compose.material3.Switch
import androidx.compose.material3.SwitchDefaults
import androidx.compose.material3.Text
import androidx.compose.material3.TopAppBar
import androidx.compose.material3.TopAppBarDefaults
import androidx.compose.material3.rememberTopAppBarState
import androidx.compose.runtime.Composable
import androidx.compose.runtime.getValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.graphics.Color
import androidx.compose.ui.input.nestedscroll.nestedScroll
import androidx.compose.ui.res.painterResource
import androidx.compose.ui.semantics.LiveRegionMode
import androidx.compose.ui.semantics.contentDescription
import androidx.compose.ui.semantics.heading
import androidx.compose.ui.semantics.liveRegion
import androidx.compose.ui.semantics.semantics
import androidx.compose.ui.semantics.Role
import androidx.compose.ui.text.font.FontWeight
import androidx.compose.ui.unit.dp
import androidx.compose.ui.unit.sp
import dev.usbdac.validator.R

/** Safety text shown before Start. Kept word for word from the original validator. */
const val SAFETY_TEXT = "Before starting: unplug headphones or mute the downstream amplifier. Fixed-level DACs can play loudly. DoP test signals can sound like loud noise on PCM-only devices. Use a quiet, known source."

class ScreenActions(
    val onStart: () -> Unit,
    val onCancel: () -> Unit,
    val onUseConnected: () -> Unit,
    val onRetryPermission: () -> Unit,
    val onUseShizuku: () -> Unit,
    val onSave: () -> Unit,
    val onShare: () -> Unit,
    val onIncludeSourceBackedChange: (Boolean) -> Unit,
    val onNotesChange: (String) -> Unit,
    val onThemeChange: (ThemeChoice) -> Unit,
)

@OptIn(ExperimentalMaterial3Api::class)
@Composable
fun ValidatorScreen(
    snapshot: CaptureSnapshot,
    includeSourceBacked: Boolean,
    notes: String,
    theme: ThemeChoice,
    dark: Boolean,
    lastOutcome: Pair<Phase, String>?,
    versionName: String,
    snackbar: SnackbarHostState,
    actions: ScreenActions,
) {
    val brand = LocalBrand.current
    val scroll = TopAppBarDefaults.pinnedScrollBehavior(rememberTopAppBarState())
    Scaffold(
        modifier = Modifier.fillMaxSize().nestedScroll(scroll.nestedScrollConnection),
        containerColor = brand.canvas,
        snackbarHost = { SnackbarHost(snackbar, Modifier.navigationBarsPadding()) },
        topBar = {
            TopAppBar(
                title = {
                    Row(verticalAlignment = Alignment.CenterVertically) {
                        Image(
                            painterResource(if (dark) R.drawable.ic_usbdac_mark_dark else R.drawable.ic_usbdac_mark_light),
                            contentDescription = null,
                            modifier = Modifier.size(28.dp),
                        )
                        Spacer(Modifier.size(10.dp))
                        Text("usbdac", style = MaterialTheme.typography.titleLarge.copy(fontWeight = FontWeight(750)))
                        Text(" Validator", style = MaterialTheme.typography.titleLarge.copy(fontWeight = FontWeight(500)), color = brand.muted)
                    }
                },
                colors = TopAppBarDefaults.topAppBarColors(containerColor = brand.canvas, scrolledContainerColor = brand.surface, titleContentColor = brand.text),
                scrollBehavior = scroll,
            )
        },
        contentWindowInsets = WindowInsets(0, 0, 0, 0),
    ) { padding ->
        Box(
            Modifier.fillMaxSize().padding(padding).consumeWindowInsets(padding).imePadding(),
            contentAlignment = Alignment.TopCenter,
        ) {
            Column(
                Modifier.widthIn(max = 640.dp).fillMaxWidth().verticalScroll(rememberScrollState()).padding(horizontal = 16.dp),
                verticalArrangement = Arrangement.spacedBy(16.dp),
            ) {
                Header()
                SafetyNotice()
                RunPanel(snapshot, includeSourceBacked, lastOutcome, actions)
                NotesPanel(notes, actions.onNotesChange)
                AnimatedVisibility(snapshot.hasArchive, enter = reveal(), exit = conceal()) {
                    ArchivePanel(enabled = snapshot.hasArchive && !snapshot.active, actions)
                }
                AppearancePanel(theme, actions.onThemeChange)
                Footer(versionName)
                Spacer(Modifier.windowInsetsBottomHeight(WindowInsets.navigationBars))
            }
        }
    }
}

private fun reveal() = fadeIn(tween(Motion.TRANSITION, easing = Motion.ease)) + expandVertically(tween(Motion.TRANSITION, easing = Motion.ease))
private fun conceal() = fadeOut(tween(Motion.FEEDBACK, easing = Motion.ease)) + shrinkVertically(tween(Motion.TRANSITION, easing = Motion.ease))

@Composable
private fun Header() {
    val brand = LocalBrand.current
    Column(Modifier.padding(top = 8.dp, bottom = 4.dp), verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Eyebrow("USB DAC validation")
        Text(
            "Inspect. Test. Understand.",
            style = MaterialTheme.typography.headlineMedium,
            color = brand.text,
            modifier = Modifier.semantics { heading() },
        )
        Text(
            "Runs exact-format USB tests on a DAC you connect and saves what it recorded as an archive.",
            style = MaterialTheme.typography.bodyLarge,
            color = brand.muted,
        )
    }
}

@Composable
private fun SafetyNotice() {
    val brand = LocalBrand.current
    Surface(
        color = brand.warningSoft,
        contentColor = brand.text,
        shape = MaterialTheme.shapes.medium,
        border = BorderStroke(1.dp, brand.warning.copy(alpha = 0.45f)),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Row(Modifier.padding(16.dp), horizontalArrangement = Arrangement.spacedBy(12.dp)) {
            Icon(painterResource(R.drawable.ic_warning), contentDescription = null, tint = brand.warning, modifier = Modifier.size(22.dp))
            Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
                Text("Protect your hearing and equipment", style = MaterialTheme.typography.titleSmall, color = brand.warning, modifier = Modifier.semantics { heading() })
                Text(SAFETY_TEXT, style = MaterialTheme.typography.bodyMedium)
            }
        }
    }
}

@Composable
private fun RunPanel(s: CaptureSnapshot, includeSourceBacked: Boolean, lastOutcome: Pair<Phase, String>?, actions: ScreenActions) {
    val brand = LocalBrand.current
    val live = phaseOf(s)
    // After saving, Capture's status becomes "Archive saved"; keep showing the run's own verdict.
    val remembered = lastOutcome?.takeIf { !s.active && !live.verdict }
    val phase = remembered?.first ?: live
    Panel {
        Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
            PanelTitle("Capture", "Validation run")
            StatusChip(phase)
        }
        when {
            remembered != null -> {
                Outcome(remembered.second)
                Text(s.status, style = MaterialTheme.typography.bodyMedium, color = brand.muted)
            }
            !s.active && live.verdict -> Outcome(s.status)
            else -> SelectionContainer {
                Text(s.status.trim(), style = MaterialTheme.typography.bodyLarge, color = brand.text, modifier = Modifier.animateContentSize(tween(Motion.TRANSITION, easing = Motion.ease)))
            }
        }
        AnimatedVisibility(s.progress != null, enter = reveal(), exit = conceal()) {
            s.progress?.let { ProgressBlock(it) }
        }
        val showConnected = s.active && s.connected.isNotEmpty() && s.canSelect
        AnimatedVisibility(showConnected, enter = reveal(), exit = conceal()) {
            FilledTonalButton(
                onClick = actions.onUseConnected,
                shape = MaterialTheme.shapes.small,
                modifier = Modifier.fillMaxWidth().heightIn(min = 48.dp),
            ) { Text(if (s.connected.size == 1) "Test connected ${s.connected.first().productName}" else "Choose a connected DAC to test") }
        }
        AnimatedVisibility(s.waitingPermission, enter = reveal(), exit = conceal()) {
            OutlinedButton(
                onClick = actions.onRetryPermission,
                shape = MaterialTheme.shapes.small,
                border = BorderStroke(1.dp, brand.controlBorder),
                modifier = Modifier.fillMaxWidth().heightIn(min = 48.dp),
            ) {
                Icon(painterResource(R.drawable.ic_refresh), contentDescription = null, modifier = Modifier.size(18.dp))
                Spacer(Modifier.size(8.dp))
                Text("Retry Android USB permission")
            }
        }
        AnimatedVisibility(s.waitingPermission && s.shizukuSupported, enter = reveal(), exit = conceal()) {
            OutlinedButton(
                onClick = actions.onUseShizuku,
                enabled = !s.shizukuBusy,
                shape = MaterialTheme.shapes.small,
                border = BorderStroke(1.dp, if (s.shizukuBusy) brand.border else brand.controlBorder),
                modifier = Modifier.fillMaxWidth().heightIn(min = 48.dp),
            ) { Text("Use Shizuku for USB access") }
        }
        HorizontalDivider(color = brand.border)
        CompatibilityRow(includeSourceBacked, enabled = !s.active, onChange = actions.onIncludeSourceBackedChange)
        Row(horizontalArrangement = Arrangement.spacedBy(12.dp), verticalAlignment = Alignment.CenterVertically) {
            Button(
                onClick = actions.onStart,
                enabled = !s.active,
                shape = MaterialTheme.shapes.small,
                contentPadding = PaddingValues(horizontal = 16.dp, vertical = 12.dp),
                modifier = Modifier.weight(1.4f).heightIn(min = 52.dp),
            ) {
                Icon(painterResource(R.drawable.ic_play), contentDescription = null, modifier = Modifier.size(20.dp))
                Spacer(Modifier.size(6.dp))
                Text("Start capture")
            }
            OutlinedButton(
                onClick = actions.onCancel,
                enabled = s.active,
                shape = MaterialTheme.shapes.small,
                border = BorderStroke(1.dp, if (s.active) brand.controlBorder else brand.border),
                modifier = Modifier.weight(1f).heightIn(min = 52.dp),
            ) { Text("Cancel") }
        }
    }
}

@Composable
private fun ProgressBlock(p: CaptureSnapshot.Progress) {
    val brand = LocalBrand.current
    val lines = p.message.lines()
    Column(verticalArrangement = Arrangement.spacedBy(8.dp)) {
        Text(lines.firstOrNull().orEmpty(), style = MaterialTheme.typography.titleSmall, color = brand.text)
        if (p.total > 0) {
            LinearProgressIndicator(
                progress = { p.completed.toFloat() / p.total },
                modifier = Modifier.fillMaxWidth().heightIn(min = 6.dp),
                color = brand.primary,
                trackColor = brand.raised,
                strokeCap = androidx.compose.ui.graphics.StrokeCap.Round,
                gapSize = 0.dp,
                drawStopIndicator = {},
            )
        } else {
            LinearProgressIndicator(
                modifier = Modifier.fillMaxWidth().heightIn(min = 6.dp),
                color = brand.primary,
                trackColor = brand.raised,
                strokeCap = androidx.compose.ui.graphics.StrokeCap.Round,
                gapSize = 0.dp,
            )
        }
        lines.drop(1).filter { it.isNotBlank() }.forEach { Text(it, style = MonoStyle, color = brand.muted) }
    }
}

@Composable
private fun Outcome(status: String) {
    val brand = LocalBrand.current
    val (headline, details) = splitOutcome(status)
    SelectionContainer {
        Column(verticalArrangement = Arrangement.spacedBy(10.dp)) {
            Text(headline, style = MaterialTheme.typography.titleMedium, color = brand.text)
            if (details.isNotEmpty()) {
                Column(
                    Modifier.fillMaxWidth().background(brand.raised.copy(alpha = if (brand.dark) 0.55f else 0.7f), RoundedCornerShape(8.dp)).padding(12.dp),
                    verticalArrangement = Arrangement.spacedBy(6.dp),
                ) { details.forEach { Text(it, style = MonoStyle, color = brand.text) } }
            }
        }
    }
}

@Composable
private fun CompatibilityRow(checked: Boolean, enabled: Boolean, onChange: (Boolean) -> Unit) {
    val brand = LocalBrand.current
    Row(
        Modifier.fillMaxWidth().heightIn(min = 48.dp).toggleable(value = checked, enabled = enabled, role = Role.Switch, onValueChange = onChange),
        verticalAlignment = Alignment.CenterVertically,
        horizontalArrangement = Arrangement.spacedBy(16.dp),
    ) {
        Column(Modifier.weight(1f), verticalArrangement = Arrangement.spacedBy(2.dp)) {
            Text("Include source-backed compatibility modes", style = MaterialTheme.typography.titleSmall, color = if (enabled) brand.text else brand.muted)
            Text("Also admits device rules taken from driver sources. These rules are not proof of hardware testing.", style = MaterialTheme.typography.bodySmall, color = brand.muted)
        }
        Switch(
            checked = checked,
            onCheckedChange = null,
            enabled = enabled,
            colors = SwitchDefaults.colors(
                checkedThumbColor = brand.onPrimary, checkedTrackColor = brand.primary,
                uncheckedThumbColor = brand.controlBorder, uncheckedTrackColor = brand.raised, uncheckedBorderColor = brand.controlBorder,
                disabledCheckedThumbColor = brand.surface, disabledCheckedTrackColor = brand.primary.copy(alpha = 0.45f),
                disabledUncheckedThumbColor = brand.border, disabledUncheckedTrackColor = brand.raised.copy(alpha = 0.6f), disabledUncheckedBorderColor = brand.border,
            ),
        )
    }
}

@Composable
private fun NotesPanel(notes: String, onChange: (String) -> Unit) {
    val brand = LocalBrand.current
    Panel {
        PanelTitle("Observation", "What did you see or hear?")
        OutlinedTextField(
            value = notes,
            onValueChange = onChange,
            modifier = Modifier.fillMaxWidth(),
            minLines = 3,
            textStyle = MaterialTheme.typography.bodyLarge,
            label = { Text("Optional observation") },
            placeholder = { Text("DAC display, audible result, or external capture") },
            supportingText = { Text("Recorded in the archive when you save or share.") },
            shape = MaterialTheme.shapes.small,
            colors = OutlinedTextFieldDefaults.colors(
                focusedBorderColor = brand.focus, unfocusedBorderColor = brand.controlBorder,
                focusedLabelColor = brand.focus, cursorColor = brand.focus,
                focusedContainerColor = brand.canvas.copy(alpha = 0.4f), unfocusedContainerColor = Color.Transparent,
            ),
        )
    }
}

@Composable
private fun ArchivePanel(enabled: Boolean, actions: ScreenActions) {
    val brand = LocalBrand.current
    Panel {
        PanelTitle("Report", "Capture archive")
        Text("A ZIP of the events, descriptors and results recorded for the latest run.", style = MaterialTheme.typography.bodyMedium, color = brand.muted)
        Column(verticalArrangement = Arrangement.spacedBy(12.dp)) {
            FilledTonalButton(onClick = actions.onSave, enabled = enabled, shape = MaterialTheme.shapes.small, modifier = Modifier.fillMaxWidth().heightIn(min = 52.dp)) {
                Icon(painterResource(R.drawable.ic_download), contentDescription = null, modifier = Modifier.size(18.dp))
                Spacer(Modifier.size(8.dp))
                Text("Save archive")
            }
            FilledTonalButton(onClick = actions.onShare, enabled = enabled, shape = MaterialTheme.shapes.small, modifier = Modifier.fillMaxWidth().heightIn(min = 52.dp)) {
                Icon(painterResource(R.drawable.ic_share), contentDescription = null, modifier = Modifier.size(18.dp))
                Spacer(Modifier.size(8.dp))
                Text("Share archive")
            }
        }
    }
}

@Composable
private fun AppearancePanel(theme: ThemeChoice, onChange: (ThemeChoice) -> Unit) {
    val brand = LocalBrand.current
    Panel {
        Text("Appearance", style = MaterialTheme.typography.titleMedium, color = brand.text, modifier = Modifier.semantics { heading() })
        SingleChoiceSegmentedButtonRow(Modifier.fillMaxWidth()) {
            ThemeChoice.entries.forEachIndexed { index, choice ->
                SegmentedButton(
                    selected = theme == choice,
                    onClick = { onChange(choice) },
                    shape = SegmentedButtonDefaults.itemShape(index, ThemeChoice.entries.size, MaterialTheme.shapes.small),
                    colors = SegmentedButtonDefaults.colors(
                        activeContainerColor = brand.primarySoft, activeContentColor = brand.text, activeBorderColor = brand.controlBorder,
                        inactiveContainerColor = Color.Transparent, inactiveContentColor = brand.muted, inactiveBorderColor = brand.controlBorder,
                    ),
                    modifier = Modifier.heightIn(min = 48.dp),
                    label = { Text(choice.label) },
                )
            }
        }
    }
}

@Composable
private fun Footer(versionName: String) {
    val brand = LocalBrand.current
    Column(Modifier.fillMaxWidth().padding(vertical = 8.dp), verticalArrangement = Arrangement.spacedBy(4.dp)) {
        Text("usbdac Validator $versionName", style = MonoStyle.copy(fontSize = 12.sp), color = brand.muted)
        Text("Direct USB audio. Clear evidence.", style = MaterialTheme.typography.bodySmall, color = brand.muted)
        Text("Manrope and IBM Plex Mono are used under the SIL Open Font License 1.1.", style = MaterialTheme.typography.bodySmall, color = brand.muted)
    }
}

@Composable
private fun Panel(content: @Composable ColumnScope.() -> Unit) {
    val brand = LocalBrand.current
    Surface(
        color = brand.surface,
        contentColor = brand.text,
        shape = MaterialTheme.shapes.medium,
        border = BorderStroke(1.dp, brand.border.copy(alpha = if (brand.dark) 0.8f else 0.6f)),
        modifier = Modifier.fillMaxWidth(),
    ) {
        Column(
            Modifier.padding(20.dp).animateContentSize(tween(Motion.TRANSITION, easing = Motion.ease)),
            verticalArrangement = Arrangement.spacedBy(14.dp),
            content = content,
        )
    }
}

@Composable
private fun PanelTitle(eyebrow: String, title: String) {
    Column(verticalArrangement = Arrangement.spacedBy(4.dp)) {
        Eyebrow(eyebrow)
        Text(title, style = MaterialTheme.typography.titleLarge, color = LocalBrand.current.text, modifier = Modifier.semantics { heading() })
    }
}

@Composable
private fun Eyebrow(text: String) {
    Text(
        text.uppercase(),
        style = MaterialTheme.typography.labelMedium.copy(letterSpacing = 1.6.sp, fontWeight = FontWeight(600)),
        color = LocalBrand.current.muted,
    )
}

@Composable
fun StatusChip(phase: Phase) {
    val brand = LocalBrand.current
    val (bg, fg) = when (phase.tone) {
        Tone.Neutral -> brand.raised to brand.muted
        Tone.Brand -> brand.primarySoft to brand.primary
        Tone.Success -> brand.successSoft to brand.success
        Tone.Warning -> brand.warningSoft to brand.warning
        Tone.Danger -> brand.dangerSoft to brand.danger
    }
    val background by animateColorAsState(bg, tween(Motion.TRANSITION, easing = Motion.ease), label = "chip")
    val foreground by animateColorAsState(fg, tween(Motion.TRANSITION, easing = Motion.ease), label = "chipText")
    Row(
        Modifier
            .background(background, RoundedCornerShape(8.dp))
            .padding(horizontal = 10.dp, vertical = 6.dp)
            .semantics(mergeDescendants = true) { liveRegion = LiveRegionMode.Polite; contentDescription = "Status: ${phase.label}" },
        verticalAlignment = Alignment.CenterVertically,
    ) {
        Box(Modifier.size(8.dp).background(foreground, CircleShape))
        Spacer(Modifier.size(6.dp))
        Text(phase.label, style = MaterialTheme.typography.labelLarge.copy(fontWeight = FontWeight(700)), color = foreground)
    }
}
