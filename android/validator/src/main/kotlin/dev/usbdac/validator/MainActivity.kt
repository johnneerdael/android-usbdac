package dev.usbdac.validator

import android.content.Context
import android.content.Intent
import android.graphics.Color
import android.net.Uri
import android.os.Bundle
import android.os.Handler
import android.os.Looper
import androidx.activity.ComponentActivity
import androidx.activity.SystemBarStyle
import androidx.activity.compose.setContent
import androidx.activity.enableEdgeToEdge
import androidx.activity.result.contract.ActivityResultContracts
import androidx.compose.foundation.layout.Column
import androidx.compose.foundation.layout.fillMaxWidth
import androidx.compose.foundation.layout.heightIn
import androidx.compose.foundation.rememberScrollState
import androidx.compose.foundation.verticalScroll
import androidx.compose.material3.AlertDialog
import androidx.compose.material3.MaterialTheme
import androidx.compose.material3.SnackbarHostState
import androidx.compose.material3.Text
import androidx.compose.material3.TextButton
import androidx.compose.runtime.Composable
import androidx.compose.runtime.LaunchedEffect
import androidx.compose.runtime.getValue
import androidx.compose.runtime.mutableStateOf
import androidx.compose.runtime.remember
import androidx.compose.runtime.saveable.rememberSaveable
import androidx.compose.runtime.setValue
import androidx.compose.ui.Alignment
import androidx.compose.ui.Modifier
import androidx.compose.ui.unit.dp
import dev.usbdac.DacDeviceInfo
import dev.usbdac.validator.ui.CaptureSnapshot
import dev.usbdac.validator.ui.LocalBrand
import dev.usbdac.validator.ui.MonoStyle
import dev.usbdac.validator.ui.Phase
import dev.usbdac.validator.ui.ScreenActions
import dev.usbdac.validator.ui.ThemeChoice
import dev.usbdac.validator.ui.ThemePreference
import dev.usbdac.validator.ui.UsbdacTheme
import dev.usbdac.validator.ui.ValidatorScreen
import dev.usbdac.validator.ui.isDark
import dev.usbdac.validator.ui.phaseOf
import androidx.lifecycle.lifecycleScope
import kotlinx.coroutines.launch

class MainActivity : ComponentActivity() {
    private val handler = Handler(Looper.getMainLooper())
    private var snapshot by mutableStateOf(read())
    /** The auto-popup chooser for several newly attached DACs (Capture.choices / Capture.choosing). */
    private var newChoices by mutableStateOf<List<DacDeviceInfo>?>(null)
    /** The "already connected" chooser opened from the Test connected / Choose button. */
    private var connectedChoices by mutableStateOf<List<DacDeviceInfo>?>(null)
    private var outcome by mutableStateOf(lastOutcome)
    private val snackbar = SnackbarHostState()

    private val saveDocument = registerForActivityResult(object : ActivityResultContracts.CreateDocument("application/zip") {
        override fun createIntent(context: Context, input: String): Intent = super.createIntent(context, input).addCategory(Intent.CATEGORY_OPENABLE)
    }) { uri ->
        if (uri != null) Capture.save(uri) { message -> showMessage(message) }
    }

    /** Same 500 ms poll as before; it now feeds Compose state instead of framework views. */
    private val refresh = object : Runnable {
        override fun run() {
            val current = read()
            snapshot = current
            val phase = phaseOf(current)
            if (!current.active && phase.verdict) { lastOutcome = phase to current.status; outcome = lastOutcome }
            val choices = Capture.choices
            if (choices.size > 1 && !Capture.choosing) {
                Capture.choosing = true
                newChoices = choices
            }
            handler.postDelayed(this, 500)
        }
    }

    private fun read(): CaptureSnapshot {
        val progress = Capture.progress()
        return CaptureSnapshot(
            status = Capture.status,
            progress = progress?.let { CaptureSnapshot.Progress(it.completed, it.total, it.message) },
            active = Capture.active,
            waitingPermission = Capture.waitingPermission,
            shizukuSupported = Capture.waitingPermission && ShizukuUsbGrant.isSupported(),
            shizukuBusy = Capture.shizukuBusy,
            connected = Capture.connected,
            canSelect = Capture.canSelect,
            hasArchive = Capture.archive != null,
        )
    }

    override fun onCreate(savedInstanceState: Bundle?) {
        super.onCreate(savedInstanceState)
        Capture.initialize(applicationContext)
        snapshot = read()
        setContent {
            var theme by remember { mutableStateOf(ThemePreference.load(this)) }
            val dark = theme.isDark()
            LaunchedEffect(dark) {
                val bars = if (dark) SystemBarStyle.dark(Color.TRANSPARENT) else SystemBarStyle.light(Color.TRANSPARENT, Color.TRANSPARENT)
                enableEdgeToEdge(statusBarStyle = bars, navigationBarStyle = bars)
            }
            var includeSourceBacked by remember { mutableStateOf(Capture.includeSourceBacked) }
            var notes by rememberSaveable { mutableStateOf("") }
            UsbdacTheme(dark) {
                ValidatorScreen(
                    snapshot = snapshot,
                    includeSourceBacked = includeSourceBacked,
                    notes = notes,
                    theme = theme,
                    dark = dark,
                    lastOutcome = outcome,
                    versionName = BuildConfig.VERSION_NAME,
                    snackbar = snackbar,
                    actions = ScreenActions(
                        onStart = {
                            lastOutcome = null; outcome = null
                            Capture.start(includeSourceBacked)
                            snapshot = read()
                        },
                        onCancel = { Capture.cancel(); snapshot = read() },
                        onUseConnected = {
                            val devices = Capture.connected
                            if (devices.isNotEmpty()) connectedChoices = devices
                        },
                        onRetryPermission = { Capture.retryPermission() },
                        onUseShizuku = { Capture.useShizuku(); snapshot = read() },
                        onSave = { Capture.observation(notes) { saveDocument.launch("usb-dac-validation.zip") } },
                        onShare = {
                            Capture.observation(notes) {
                                val uri = Uri.parse("content://$packageName.archive/${requireNotNull(Capture.archive).name}")
                                startActivity(Intent.createChooser(Intent(Intent.ACTION_SEND).setType("application/zip").putExtra(Intent.EXTRA_STREAM, uri).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION), "Share validation archive"))
                            }
                        },
                        onIncludeSourceBackedChange = { checked -> includeSourceBacked = checked; Capture.includeSourceBacked = checked },
                        onNotesChange = { notes = it },
                        onThemeChange = { choice -> theme = choice; ThemePreference.store(this, choice) },
                    ),
                )
                newChoices?.let { choices ->
                    DacChooser(
                        title = "Choose the newly attached DAC",
                        devices = choices,
                        onSelect = { device -> newChoices = null; Capture.select(device) },
                        onDismiss = { newChoices = null; Capture.choosing = false },
                        dismissLabel = null,
                    )
                }
                connectedChoices?.let { devices ->
                    DacChooser(
                        title = "Select an already connected DAC",
                        devices = devices,
                        onSelect = { device -> connectedChoices = null; Capture.selectConnected(device) },
                        onDismiss = { connectedChoices = null },
                        dismissLabel = "Cancel",
                    )
                }
            }
        }
    }

    private fun showMessage(message: String) { lifecycleScope.launch { snackbar.showSnackbar(message) } }

    override fun onResume() { super.onResume(); Capture.windowResumed(hasWindowFocus()); handler.post(refresh) }
    override fun onPause() { handler.removeCallbacks(refresh); Capture.choosing = false; Capture.windowPaused(isChangingConfigurations); super.onPause() }
    override fun onWindowFocusChanged(hasFocus: Boolean) { super.onWindowFocusChanged(hasFocus); Capture.windowFocusChanged(hasFocus) }

    private companion object {
        /** The last finished run's verdict, kept while Capture's status moves on to "Archive saved". */
        var lastOutcome: Pair<Phase, String>? = null
    }
}

@Composable
private fun DacChooser(title: String, devices: List<DacDeviceInfo>, onSelect: (DacDeviceInfo) -> Unit, onDismiss: () -> Unit, dismissLabel: String?) {
    val brand = LocalBrand.current
    AlertDialog(
        onDismissRequest = onDismiss,
        title = { Text(title) },
        text = {
            Column(Modifier.verticalScroll(rememberScrollState())) {
                devices.forEach { device ->
                    TextButton(
                        onClick = { onSelect(device) },
                        modifier = Modifier.fillMaxWidth().heightIn(min = 56.dp),
                        shape = MaterialTheme.shapes.small,
                    ) {
                        Column(Modifier.fillMaxWidth(), horizontalAlignment = Alignment.Start) {
                            Text(device.productName, style = MaterialTheme.typography.titleSmall, color = brand.text)
                            Text("${device.vendorId.toString(16)}:${device.productId.toString(16)}", style = MonoStyle, color = brand.muted)
                        }
                    }
                }
            }
        },
        confirmButton = { if (dismissLabel != null) TextButton(onClick = onDismiss) { Text(dismissLabel) } },
        containerColor = brand.surface,
        titleContentColor = brand.text,
        shape = MaterialTheme.shapes.large,
    )
}
