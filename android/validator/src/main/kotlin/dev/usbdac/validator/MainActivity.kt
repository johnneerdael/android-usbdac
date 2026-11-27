package dev.usbdac.validator

import android.app.Activity
import android.app.AlertDialog
import android.content.*
import android.database.Cursor
import android.database.MatrixCursor
import android.hardware.usb.UsbDevice
import android.hardware.usb.UsbManager
import android.net.Uri
import android.os.*
import android.provider.OpenableColumns
import android.view.ViewGroup
import android.widget.*
import dev.usbdac.*
import org.json.JSONObject
import java.io.File
import java.util.concurrent.Executors
import java.util.zip.ZipEntry
import java.util.zip.ZipOutputStream

class MainActivity : Activity() {
    private lateinit var status: TextView
    private lateinit var progressBar: ProgressBar
    private lateinit var progressLabel: TextView
    private lateinit var useConnected: Button
    private lateinit var retryPermission: Button
    private lateinit var shizukuPermission: Button
    private lateinit var compatibility: CheckBox
    private lateinit var start: Button
    private lateinit var cancel: Button
    private lateinit var save: Button
    private lateinit var share: Button
    private lateinit var notes: EditText
    private val refresh = object : Runnable {
        override fun run() {
            val progress = Capture.progress()
            status.text = Capture.status
            progressBar.visibility = if (progress != null) android.view.View.VISIBLE else android.view.View.GONE
            progressLabel.visibility = progressBar.visibility
            if (progress != null) {
                progressBar.isIndeterminate = progress.total <= 0
                if (progress.total > 0) { progressBar.max = progress.total; progressBar.progress = progress.completed }
                progressLabel.text = progress.message
            }
            retryPermission.visibility = if (Capture.waitingPermission) android.view.View.VISIBLE else android.view.View.GONE
            shizukuPermission.visibility = if (Capture.waitingPermission && ShizukuUsbGrant.isSupported()) android.view.View.VISIBLE else android.view.View.GONE
            shizukuPermission.isEnabled = !Capture.shizukuBusy
            useConnected.visibility = if (Capture.active && Capture.connected.isNotEmpty() && Capture.canSelect) android.view.View.VISIBLE else android.view.View.GONE
            useConnected.text = if (Capture.connected.size == 1) "Test connected ${Capture.connected.first().productName}" else "Choose a connected DAC to test"
            compatibility.isEnabled = !Capture.active
            start.isEnabled = !Capture.active
            cancel.isEnabled = Capture.active
            save.isEnabled = Capture.archive != null && !Capture.active
            share.isEnabled = save.isEnabled
            val choices = Capture.choices
            if (choices.size > 1 && !Capture.choosing) {
                Capture.choosing = true
                AlertDialog.Builder(this@MainActivity).setTitle("Choose the newly attached DAC")
                    .setItems(choices.map { "${it.productName} (${it.vendorId.toString(16)}:${it.productId.toString(16)})" }.toTypedArray()) { _, index -> Capture.select(choices[index]) }
                    .setOnCancelListener { Capture.choosing = false }.show()
            }
            status.postDelayed(this, 500)
        }
    }
    override fun onCreate(state: Bundle?) {
        super.onCreate(state)
        Capture.initialize(applicationContext)
        val layout = LinearLayout(this).apply { orientation = LinearLayout.VERTICAL; setPadding(28, 28, 28, 28) }
        val scroll = ScrollView(this); scroll.addView(layout)
        fun label(text: String) = TextView(this).apply { this.text = text; textSize = 16f; layout.addView(this) }
        label("USB DAC validator")
        label("Before starting: unplug headphones or mute the downstream amplifier. Fixed-level DACs can play loudly. DoP test signals can sound like loud noise on PCM-only devices. Use a quiet, known source.")
        status = label("")
        progressBar = ProgressBar(this, null, android.R.attr.progressBarStyleHorizontal).apply { visibility = android.view.View.GONE; layout.addView(this, ViewGroup.LayoutParams(-1, -2)) }
        progressLabel = label("")
        notes = EditText(this).apply { hint = "Optional observation: DAC display, audible result, or external capture"; layout.addView(this, ViewGroup.LayoutParams(-1, -2)) }
        fun button(text: String, action: () -> Unit) = Button(this).apply { this.text = text; setOnClickListener { action() }; layout.addView(this) }
        compatibility = CheckBox(this).apply {
            text = "Include source-backed compatibility modes"
            isChecked = Capture.includeSourceBacked
            setOnCheckedChangeListener { _, checked -> Capture.includeSourceBacked = checked }
            layout.addView(this)
        }
        start = button("Start") { Capture.start(compatibility.isChecked) }
        useConnected = button("Use an already connected DAC") {
            val devices = Capture.connected
            if (devices.isNotEmpty()) AlertDialog.Builder(this).setTitle("Select an already connected DAC")
                .setItems(devices.map { "${it.productName} (${it.vendorId.toString(16)}:${it.productId.toString(16)})" }.toTypedArray()) { _, index -> Capture.selectConnected(devices[index]) }.setNegativeButton("Cancel", null).show()
        }
        retryPermission = button("Retry Android USB permission") { Capture.retryPermission() }
        shizukuPermission = button("Use Shizuku for USB access") { Capture.useShizuku() }
        cancel = button("Cancel") { Capture.cancel() }
        save = button("Save archive") { Capture.observation(notes.text.toString()) { startActivityForResult(Intent(Intent.ACTION_CREATE_DOCUMENT).addCategory(Intent.CATEGORY_OPENABLE).setType("application/zip").putExtra(Intent.EXTRA_TITLE, "usb-dac-validation.zip"), 1) } }
        share = button("Share archive") {
            Capture.observation(notes.text.toString()) {
                val uri = Uri.parse("content://$packageName.archive/${requireNotNull(Capture.archive).name}")
                startActivity(Intent.createChooser(Intent(Intent.ACTION_SEND).setType("application/zip").putExtra(Intent.EXTRA_STREAM, uri).addFlags(Intent.FLAG_GRANT_READ_URI_PERMISSION), "Share validation archive"))
            }
        }
        setContentView(scroll)
    }
    override fun onResume() { super.onResume(); Capture.windowResumed(hasWindowFocus()); status.post(refresh) }
    override fun onPause() { status.removeCallbacks(refresh); Capture.choosing = false; Capture.windowPaused(isChangingConfigurations); super.onPause() }
    override fun onWindowFocusChanged(hasFocus: Boolean) { super.onWindowFocusChanged(hasFocus); Capture.windowFocusChanged(hasFocus) }
    @Deprecated("Activity result API") override fun onActivityResult(requestCode: Int, resultCode: Int, data: Intent?) {
        super.onActivityResult(requestCode, resultCode, data)
        if (requestCode == 1 && resultCode == RESULT_OK) data?.data?.let { uri ->
            Capture.save(uri) { message -> Toast.makeText(this, message, Toast.LENGTH_LONG).show() }
        }
    }
}

class ArchiveProvider : ContentProvider() {
    override fun onCreate() = true
    override fun getType(uri: Uri) = "application/zip"
    override fun query(uri: Uri, projection: Array<out String>?, selection: String?, selectionArgs: Array<out String>?, sortOrder: String?): Cursor = MatrixCursor(arrayOf(OpenableColumns.DISPLAY_NAME, OpenableColumns.SIZE)).apply { addRow(arrayOf<Any>("usb-dac-validation.zip", Capture.archive?.length() ?: 0L)) }
    override fun openFile(uri: Uri, mode: String): ParcelFileDescriptor {
        require(mode == "r")
        val name = requireNotNull(uri.lastPathSegment)
        require(name.matches(Regex("archive-[0-9]+-[0-9]+\\.zip")))
        return ParcelFileDescriptor.open(File(requireNotNull(context).filesDir, name), ParcelFileDescriptor.MODE_READ_ONLY)
    }
    override fun insert(uri: Uri, values: ContentValues?): Uri? = null
    override fun delete(uri: Uri, selection: String?, selectionArgs: Array<out String>?) = 0
    override fun update(uri: Uri, values: ContentValues?, selection: String?, selectionArgs: Array<out String>?) = 0
}

private object Capture {
    private lateinit var app: Context
    private val main = Handler(Looper.getMainLooper())
    private val worker = Executors.newSingleThreadExecutor()
    private lateinit var directory: File
    private var baseline = emptySet<String>()
    @Volatile private var selected: DacDeviceInfo? = null
    private var receiver: BroadcastReceiver? = null
    private var permissionReceiver: BroadcastReceiver? = null
    private var grant: ShizukuUsbGrant? = null
    @Volatile var shizukuBusy = false
    val waitingPermission: Boolean get() = active && !finishing && permissionPending && selected != null
    private var permissionAttempt = 0
    private var nextPermissionRequest = 0
    private var windowForeground = false
    private var windowFocused = false
    private var requestSawFocusLoss = false
    private var autoRetried = false
    private var explicitPermissionDenial = false
    private var focusRetry: Runnable? = null
    @Volatile private var generation = 0L
    private var permissionAnswered = false
    @Volatile private var permissionPending = false
    @Volatile private var permissionNote = ""
    private var nativeStarted = false
    @Volatile private var finishing = false
    @Volatile var includeSourceBacked = true
    private var runPolicy = QuirkPolicy.SOURCE_BACKED
    @Volatile var active = false
    @Volatile var status = "Ready. Start, then connect a DAC."
    @Volatile var archive: File? = null
    private var archiveBelongsToDirectory = false
    private var archiveWarning: String? = null
    @Volatile var choices: List<DacDeviceInfo> = emptyList()
    @Volatile var connected: List<DacDeviceInfo> = emptyList()
    val canSelect: Boolean get() = selected == null && !finishing
    var choosing = false
    @Volatile private var cancelled = false
    @Volatile private var deviceLost = false
    private const val PERMISSION = "dev.usbdac.validator.USB_PERMISSION"
    private val timeout = Runnable { if (active && selected == null) finish("Timed out waiting for a new DAC") }
    data class DisplayProgress(val completed: Int, val total: Int, val message: String)
    fun progress(): DisplayProgress? {
        if (!::directory.isInitialized || !active || finishing) return null
        val value = runCatching { JSONObject(File(directory, "progress.json").readText()) }.getOrNull() ?: return null
        val total = value.optInt("total").coerceAtLeast(0)
        val index = value.optInt("index").coerceAtLeast(0)
        val phase = value.optString("phase")
        if (total == 0 || (phase != "running" && phase != "probing")) return null
        val leg = value.optString("leg")
        fun field(name: String): String? = Regex("(?:^|[,{]\\s*)" + name + ":\\s*([^,}]+)").find(leg)?.groupValues?.get(1)?.trim()
        val rate = field("sample_rate")?.toIntOrNull()?.takeIf { it > 0 }
        val dsdRate = field("bit_rate")?.toIntOrNull()
        val rawEncoding = field("encoding") ?: "audio"
        val encoding = rawEncoding.substringBefore(" {")
        val bits = field("bit_depth")
        val channels = field("channels")
        val details = listOfNotNull(encoding, (dsdRate ?: rate)?.let { "$it Hz" }, bits?.takeUnless { it == "0" }?.let { "$it-bit" }, channels?.let { "$it channels" }).joinToString(" · ")
        return DisplayProgress(index.coerceAtMost(total), total, "${if (phase == "probing") "Checking admission" else "Testing"} ${index + 1} of $total\n$details")
    }
    fun initialize(context: Context) {
        if (::app.isInitialized) return
        app = context
        archive = app.filesDir.listFiles()?.filter { it.name.matches(Regex("archive-[0-9]+-[0-9]+\\.zip")) }?.maxByOrNull { it.lastModified() }
        if (archive != null) status = "Previous archive available to save or share."
        val pending = app.filesDir.listFiles()?.filter { it.isDirectory && it.name.startsWith("run-") && File(it, "pending.json").isFile }
            ?.sortedByDescending { it.name }.orEmpty()
        val interrupted = pending.firstOrNull { dir ->
            val marker = runCatching { JSONObject(File(dir, "pending.json").readText()) }.getOrNull()
            val expected = marker?.optString("published_archive").orEmpty()
            val published = expected.takeIf { it.matches(Regex("archive-[0-9]+-[0-9]+\\.zip")) }?.let { File(app.filesDir, it) }
            if (published?.isFile == true) { File(dir, "pending.json").delete(); false } else true
        } ?: return
        directory = interrupted
        active = true; finishing = true; nativeStarted = false; archiveBelongsToDirectory = false
        status = "Recovering an interrupted capture…"
        worker.execute {
            val result = runCatching {
                record("finished", JSONObject().put("status", "interrupted").put("reason", "app process stopped before archive publication"))
                File(directory, "interrupted.json").writeText(JSONObject().put("status", "interrupted").put("reason", "app process stopped before archive publication").toString())
                archive()
            }
            main.post {
                active = false; finishing = false
                result.onSuccess { status = "Interrupted capture recovered. Save or share the archive.${warningSuffix()}" }
                    .onFailure { status = "Interrupted capture recovery failed: ${it.message}. ${previousArchiveMessage()}" }
            }
        }
    }
    private fun previousArchiveMessage(): String = if (archive != null) "The previous archive is still available to save or share." else "The raw capture remains on this device for recovery."
    private fun warningSuffix(): String = archiveWarning?.let { " Warning: $it" } ?: ""
    private fun markerFile(): File = File(directory, "pending.json")
    private fun writeMarker(value: JSONObject) {
        val marker = markerFile()
        val temporary = File(directory, "pending.json.tmp")
        temporary.writeText(value.toString())
        check(temporary.renameTo(marker)) { "Cannot persist capture state" }
    }
    @Synchronized private fun record(kind: String, data: JSONObject = JSONObject()) {
        if (!::directory.isInitialized) return
        runCatching { File(directory, "android-events.jsonl").appendText(JSONObject().put("time_ms", System.currentTimeMillis()).put("kind", kind).put("data", data).toString() + "\n") }
    }
    fun start(includeCompatibility: Boolean) {
        if (active) return
        runPolicy = if (includeCompatibility) QuirkPolicy.SOURCE_BACKED else QuirkPolicy.VERIFIED_ONLY
        directory = File(app.filesDir, "run-${System.currentTimeMillis()}-${System.nanoTime()}")
        archiveBelongsToDirectory = false
        try {
            check(directory.mkdirs()) { "Cannot create capture directory" }
            writeMarker(JSONObject().put("state", "pending").put("created_ms", System.currentTimeMillis()))
        } catch (e: Throwable) { status = "Cannot start capture: ${e.message}. ${previousArchiveMessage()}"; return }
        generation++; val runId = generation
        cancelled = false; deviceLost = false; active = true; choices = emptyList(); selected = null; choosing = false; permissionAnswered = false; permissionPending = false; permissionAttempt = 0; requestSawFocusLoss = false; autoRetried = false; explicitPermissionDenial = false; shizukuBusy = false; grant = null; permissionNote = ""; connected = emptyList(); nativeStarted = false; finishing = false
        archiveBelongsToDirectory = false; archiveWarning = null
        record("capture_started", JSONObject().put("sdk", Build.VERSION.SDK_INT).put("manufacturer", Build.MANUFACTURER).put("model", Build.MODEL).put("app_version", runCatching { app.packageManager.getPackageInfo(app.packageName, 0).versionName }.getOrNull()).put("quirk_policy", runPolicy.name).put("require_hardware_volume", false).put("capture_feedback_trace", true).put("startup_volume", "KEEP"))
        try { ValidatorBridge.begin(directory.absolutePath); nativeStarted = true; record("driver_version", JSONObject().put("version", UsbDac.nativeVersion)) } catch (e: Throwable) { failure("native_begin", e); finish("Cannot start native capture"); return }
        receiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context, intent: Intent) {
                if (runId != generation || !active) return
                val device: UsbDevice? = if (Build.VERSION.SDK_INT >= 33) intent.getParcelableExtra(UsbManager.EXTRA_DEVICE, UsbDevice::class.java) else @Suppress("DEPRECATION") intent.getParcelableExtra(UsbManager.EXTRA_DEVICE)
                val info = device?.let { runCatching { UsbDac.describe(it) }.getOrNull() }
                when (intent.action) {
                    UsbManager.ACTION_USB_DEVICE_ATTACHED -> if (info == null) { record("unrecognized_usb_attach") } else if (info.deviceName !in baseline && selected == null) {
                        record("attached", identity(info)); offer(info, runId)
                    }
                    UsbManager.ACTION_USB_DEVICE_DETACHED -> if (device != null) {
                        if (info != null) record("detached", identity(info)) else record("detached", JSONObject().put("device_name", device.deviceName))
                        baseline = baseline - device.deviceName; choices = choices.filterNot { it.deviceName == device.deviceName }
                        if (selected?.deviceName == device.deviceName) { deviceLost = true; record("selected_device_detached", JSONObject().put("device_name", device.deviceName)); runCatching { ValidatorBridge.deviceLost() }; if (permissionPending) finish("Selected DAC disconnected before claim") else status = "Selected DAC disconnected; stopping safely…" }
                    }

                }
            }
        }
        val filter = IntentFilter().apply { addAction(UsbManager.ACTION_USB_DEVICE_ATTACHED); addAction(UsbManager.ACTION_USB_DEVICE_DETACHED) }
        try {
            if (Build.VERSION.SDK_INT >= 33) app.registerReceiver(receiver, filter, Context.RECEIVER_EXPORTED) else @Suppress("DEPRECATION") app.registerReceiver(receiver, filter)
        } catch (e: Throwable) { failure("register_receiver", e); finish("USB receiver failed"); return }
        connected = UsbDac.attachedDacs(app)
        baseline = connected.map { it.deviceName }.toSet()
        record("baseline", JSONObject().put("attached_dac_count", baseline.size))
        status = if (connected.isEmpty()) "Capture started. Connect your DAC now; tests will start automatically after USB permission." else "Capture started. ${connected.size} DAC(s) already connected. Reconnect your DAC to capture attachment, or choose Use an already connected DAC."
        main.postDelayed(timeout, 120_000)
        poll(runId)
    }
    private fun offer(info: DacDeviceInfo, runId: Long) {
        if (!active || finishing || runId != generation || selected != null) return
        choices = (choices + info).distinctBy { it.deviceName }
        status = "Found ${info.productName}. Preparing USB access…"
        main.postDelayed({ if (runId == generation && active && !finishing && selected == null && choices.size == 1) select(choices.first()) }, 1200)
    }
    private fun poll(runId: Long) {
        if (!active || finishing || runId != generation || selected != null) return
        runCatching { UsbDac.attachedDacs(app) }.onSuccess { devices ->
            connected = devices.filter { it.deviceName in baseline }
            devices.filter { it.deviceName !in baseline && choices.none { c -> c.deviceName == it.deviceName } }.forEach { record("poll_discovered", identity(it)); offer(it, runId) }
            choices = choices.filter { candidate -> devices.any { it.deviceName == candidate.deviceName } }
        }.onFailure { failure("usb_poll", it) }
        main.postDelayed({ poll(runId) }, 1000)
    }
    fun selectConnected(info: DacDeviceInfo) {
        if (!active || finishing || selected != null || connected.none { it.deviceName == info.deviceName }) return
        record("selected_existing_device", identity(info))
        choices = listOf(info)
        select(info)
    }
    fun select(info: DacDeviceInfo) {
        if (!active || selected != null || info.deviceName !in choices.map { it.deviceName }) return
        selected = info; choosing = false; choices = emptyList(); main.removeCallbacks(timeout)
        record("selected", identity(info)); status = "Found ${info.productName}. Checking USB permission…"
        val manager = app.getSystemService(Context.USB_SERVICE) as UsbManager
        val runId = generation
        if (manager.hasPermission(info.device)) {
            permissionNote = "USB permission was already granted; Android did not need another dialog."
            record("permission", JSONObject().put("granted", true).put("already_granted", true)); permissionGranted(info, "existing")
        } else {
            permissionPending = true
            requestAndroidPermission(info, runId)
            pollPermission(runId, info)
        }
    }
    fun retryPermission() {
        val info = selected ?: return
        if (!waitingPermission) return
        explicitPermissionDenial = false
        cancelFocusRetry()
        requestAndroidPermission(info, generation)
    }
    fun windowResumed(hasFocus: Boolean) {
        windowForeground = true
        windowFocusChanged(hasFocus)
    }
    fun windowPaused(changingConfigurations: Boolean) {
        windowForeground = false
        cancelFocusRetry()
        if (changingConfigurations) { windowFocused = false; requestSawFocusLoss = false }
    }
    fun windowFocusChanged(hasFocus: Boolean) {
        windowFocused = hasFocus
        if (!waitingPermission) return
        record("permission_window_focus", JSONObject().put("focused", hasFocus).put("foreground", windowForeground))
        if (!hasFocus) { requestSawFocusLoss = true; cancelFocusRetry(); return }
        if (!windowForeground || !requestSawFocusLoss || autoRetried || explicitPermissionDenial || shizukuBusy) return
        cancelFocusRetry()
        val runId = generation
        val attempt = permissionAttempt
        val candidate = selected ?: return
        val retry = Runnable {
            focusRetry = null
            if (runId != generation || !waitingPermission || permissionAttempt != attempt || !windowForeground || !windowFocused || !requestSawFocusLoss || autoRetried || explicitPermissionDenial || shizukuBusy) return@Runnable
            val manager = app.getSystemService(Context.USB_SERVICE) as UsbManager
            val current = manager.deviceList[candidate.deviceName]
            if (current == null || current.deviceId != candidate.device.deviceId || current.vendorId != candidate.vendorId || current.productId != candidate.productId) return@Runnable
            if (manager.hasPermission(current)) { permissionGranted(candidate, "Android permission check"); return@Runnable }
            autoRetried = true; requestSawFocusLoss = false
            record("permission_auto_retry", JSONObject().put("reason", "validator_window_regained_focus").put("previous_attempt", attempt))
            requestAndroidPermission(candidate, runId)
        }
        focusRetry = retry
        main.postDelayed(retry, 700)
    }
    private fun cancelFocusRetry() { focusRetry?.let(main::removeCallbacks); focusRetry = null }
    private fun requestAndroidPermission(info: DacDeviceInfo, runId: Long) {
        if (!waitingPermission) return
        val attempt = ++permissionAttempt
        requestSawFocusLoss = !windowFocused
        record("permission_request_focus", JSONObject().put("attempt", attempt).put("focused", windowFocused).put("foreground", windowForeground))
        status = "Waiting for Android USB permission for ${info.productName}. If no dialog appears, retry or cancel to save the archive."
        record("permission_requested", JSONObject().put("attempt", attempt))
        permissionReceiver?.let { runCatching { app.unregisterReceiver(it) } }; permissionReceiver = null
        val manager = app.getSystemService(Context.USB_SERVICE) as UsbManager
        val current = manager.deviceList[info.deviceName]
        if (current == null || current.deviceId != info.device.deviceId || current.vendorId != info.vendorId || current.productId != info.productId) {
            record("permission_device_lost"); finish("Selected DAC disconnected or changed"); return
        }
        if (manager.hasPermission(current)) { permissionGranted(info, "Android permission check"); return }
        val requestId = ++nextPermissionRequest
        val localReceiver = object : BroadcastReceiver() {
            override fun onReceive(context: Context, intent: Intent) {
                if (intent.action != PERMISSION || intent.getIntExtra("request_id", -1) != requestId || runId != generation || permissionAttempt != attempt || !waitingPermission) return
                val returned: UsbDevice? = if (Build.VERSION.SDK_INT >= 33) intent.getParcelableExtra(UsbManager.EXTRA_DEVICE, UsbDevice::class.java) else @Suppress("DEPRECATION") intent.getParcelableExtra(UsbManager.EXTRA_DEVICE)
                val attached = manager.deviceList[info.deviceName]
                val granted = intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false) && returned?.deviceName == info.deviceName && attached != null && attached.deviceId == info.device.deviceId && attached.vendorId == info.vendorId && attached.productId == info.productId && manager.hasPermission(attached)
                record("permission_result", JSONObject().put("attempt", attempt).put("confirmed", granted).put("callback_granted", intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false)))
                permissionReceiver?.let { runCatching { app.unregisterReceiver(it) } }; permissionReceiver = null
                if (granted) permissionGranted(info, "Android")
                else { explicitPermissionDenial = intent.hasExtra(UsbManager.EXTRA_PERMISSION_GRANTED) && !intent.getBooleanExtra(UsbManager.EXTRA_PERMISSION_GRANTED, false); cancelFocusRetry(); status = "Android did not confirm USB permission. Retry or cancel to save the archive." }
            }
        }
        try {
            if (Build.VERSION.SDK_INT >= 33) app.registerReceiver(localReceiver, IntentFilter(PERMISSION), Context.RECEIVER_NOT_EXPORTED) else @Suppress("DEPRECATION") app.registerReceiver(localReceiver, IntentFilter(PERMISSION))
            permissionReceiver = localReceiver
            val flags = android.app.PendingIntent.FLAG_ONE_SHOT or if (Build.VERSION.SDK_INT >= 31) android.app.PendingIntent.FLAG_MUTABLE else 0
            val intent = Intent(PERMISSION).setPackage(app.packageName).putExtra("request_id", requestId)
            manager.requestPermission(current, android.app.PendingIntent.getBroadcast(app, requestId, intent, flags))
        } catch (e: Throwable) {
            permissionReceiver?.let { runCatching { app.unregisterReceiver(it) } }; permissionReceiver = null
            failure("permission_request", e); status = "Android permission request failed: ${e.message}. Retry or cancel to save."
        }
        main.postDelayed({ if (runId == generation && active && !permissionAnswered && permissionAttempt == attempt) { record("permission_timed_out"); finish("USB permission timed out; no confirmed access") } }, 120_000)
    }
    private fun pollPermission(runId: Long, info: DacDeviceInfo) {
        if (runId != generation || !waitingPermission || permissionAnswered) return
        val manager = app.getSystemService(Context.USB_SERVICE) as UsbManager
        val current = manager.deviceList[info.deviceName]
        if (current == null || current.deviceId != info.device.deviceId || current.vendorId != info.vendorId || current.productId != info.productId) { record("permission_device_lost"); finish("Selected DAC disconnected or changed"); return }
        if (manager.hasPermission(current)) { permissionGranted(info, "Android permission check"); return }
        main.postDelayed({ pollPermission(runId, info) }, 1000)
    }
    private fun permissionGranted(info: DacDeviceInfo, source: String) {
        if (!active || finishing || permissionAnswered || selected?.deviceName != info.deviceName) return
        permissionAnswered = true; permissionPending = false; cancelFocusRetry()
        permissionReceiver?.let { runCatching { app.unregisterReceiver(it) } }; permissionReceiver = null
        grant?.cancel(); grant = null; shizukuBusy = false
        if (source != "existing" && source != "privileged manager") permissionNote = "USB permission confirmed via $source."
        record("permission_confirmed", JSONObject().put("source", source))
        run(info)
    }
    fun useShizuku() {
        val info = selected ?: return
        if (!waitingPermission || shizukuBusy) return
        val runId = generation
        cancelFocusRetry(); shizukuBusy = true; status = "Checking Shizuku for ${info.productName}; approve its access prompt if shown…"
        grant = ShizukuUsbGrant(app, info.device,
            { runId == generation && active && !finishing && permissionPending && selected?.deviceName == info.deviceName },
            { stage, detail -> record("privileged_$stage", JSONObject().put("detail", detail)) },
            { ok, message ->
                if (runId == generation && waitingPermission) {
                    shizukuBusy = false; grant = null
                    if (ok) { permissionNote = message; permissionGranted(info, "privileged manager") }
                    else status = message
                }
            }).also { it.start() }
    }

    private fun identity(info: DacDeviceInfo): JSONObject = JSONObject().put("vendor_id", info.vendorId).put("product_id", info.productId).put("product", info.productName).put("manufacturer", info.manufacturerName).put("configurations", org.json.JSONArray(info.configurations.map { JSONObject().put("id", it.id).put("name", it.name).put("playback", it.hasPlayback).put("capture", it.hasCapture) }))
    private fun run(info: DacDeviceInfo) {
        status = "USB permission granted for ${info.productName}. Reading USB descriptors…"
        val runId = generation
        worker.execute {
            var session: DacSession? = null
            var outcome = "Validation failed"
            try {
                val manager = app.getSystemService(Context.USB_SERVICE) as UsbManager
                val connection = manager.openDevice(info.device) ?: error("Cannot open USB device for descriptors")
                try {
                    File(directory, "usb-descriptors.bin").writeBytes(connection.rawDescriptors ?: byteArrayOf())
                    record("descriptors", JSONObject().put("bytes", File(directory, "usb-descriptors.bin").length()))
                } finally { connection.close() }
                if (deviceLost) error("Selected DAC disconnected")
                if (cancelled) error("Cancelled")
                status = "Claiming exclusive USB audio interfaces for ${info.productName}…"
                record("claim_started", identity(info))
                session = UsbDac.open(app, info, DacConfig(requireHardwareVolume = false, captureFeedbackTrace = true, startupVolume = StartupVolume.Keep, quirkPolicy = runPolicy))
                val capabilities = session.capabilities
                record("recognized_capabilities", JSONObject().put("quirk_policy", runPolicy.name)
                    .put("supports_pcm", capabilities.supportsPcm).put("pcm_sample_rates", org.json.JSONArray(capabilities.pcmSampleRates))
                    .put("pcm_bit_depths", org.json.JSONArray(capabilities.pcmBitDepths)).put("supports_dop", capabilities.supportsDop)
                    .put("dop_max_dsd_bit_rate", capabilities.dopMaxDsdBitRate).put("dop_decoder", "not_observed")
                    .put("supports_native_dsd", capabilities.supportsNativeDsd).put("native_dsd_bit_rates", org.json.JSONArray(capabilities.nativeDsdBitRates)))
                val claim = JSONObject(session.validatorClaimEvidenceJson())
                record("claim_confirmed", claim)
                status = "Exclusive USB audio claim confirmed for ${info.productName} (configuration ${claim.optInt("configuration_id")}, interfaces ${claim.optJSONArray("claimed_interface_ids")}). Preparing tests…"
                record("sdk_opened", JSONObject().put("capabilities", session.capabilities.toString()).put("startup_volume", "keep").put("fixed_output_allowed", true))
                if (deviceLost) error("Selected DAC disconnected")
                if (cancelled) error("Cancelled")
                status = "$permissionNote\nExclusive USB audio claim confirmed for ${info.productName} (configuration ${claim.optInt("configuration_id")}, interfaces ${claim.optJSONArray("claimed_interface_ids")}). Running tests…"
                val report = session.runValidator(directory.absolutePath)
                val result = JSONObject(report)
                record("suite_result", result)
                val legs = result.optJSONArray("legs")
                val passed = (0 until (legs?.length() ?: 0)).count { legs?.optJSONObject(it)?.optString("status") == "passed_usb" }
                val count = legs?.length() ?: 0
                outcome = when (result.optString("status")) { "passed_usb" -> "USB validation passed ($passed of $count cases)"; "cancelled" -> "Cancelled"; "device_lost" -> "Selected DAC lost during validation"; "no_cases" -> "No test cases available"; "failed" -> "Validation failed: ${result.optString("error", "unknown error")}"; else -> "Validation completed with findings ($passed of $count cases passed); see archive" }
                result.optJSONArray("admission_results")?.let { probes ->
                    val unexpected = (0 until probes.length()).count { probes.optJSONObject(it)?.optString("admission") == "unexpected_admitted" }
                    val inconclusive = (0 until probes.length()).count { probes.optJSONObject(it)?.optString("admission") == "inconclusive" }
                    outcome += "\nAdmission checks: ${probes.length()} attempted, $unexpected unexpectedly admitted, $inconclusive inconclusive."
                }
                result.optJSONObject("mode_coverage")?.let { coverage ->
                    val lines = listOf("pcm" to "PCM", "dop" to "DoP", "native_dsd" to "Native DSD").map { (key, label) ->
                        val item = coverage.optJSONObject(key)
                        if (item == null || item.optInt("planned") == 0) "$label: no admitted cases"
                        else "$label: ${item.optInt("exercised")} of ${item.optInt("planned")} cases exercised, ${item.optInt("passed")} passed"
                    }
                    outcome += "\n" + lines.joinToString("\n") + "\nDoP decoder lock was not observed."
                }
            } catch (e: Throwable) { failure("open_or_run", e); outcome = if (deviceLost) "Selected DAC disconnected" else if (cancelled) "Cancelled" else "Validation failed: ${e.message}" }
            finally {
                runCatching { session?.feedbackTraceJson()?.let { File(directory, "feedback-trace.json").writeText(it) } }
                runCatching { session?.close(); record("session_close", JSONObject().put("status", "closed")) }.onFailure { failure("session_close", it); outcome = "Validation cleanup failed" }
                main.post { if (runId == generation) finish(if (deviceLost) "Selected DAC disconnected" else if (cancelled) "Cancelled" else outcome) }
            }
        }
    }
    fun cancel() {
        if (!active || finishing) return
        cancelled = true; record("cancel_requested"); runCatching { ValidatorBridge.cancel() }
        if (selected == null || permissionPending) finish("Cancelled") else status = "Cancelling and releasing DAC…"
    }
    private fun failure(stage: String, e: Throwable) {
        record("error", JSONObject().put("stage", stage).put("type", e.javaClass.simpleName).put("code", (e as? DacException)?.code?.toString()).put("message", e.message))
    }
    fun observation(text: String, done: () -> Unit) {
        if (text.isBlank() || !::directory.isInitialized || !archiveBelongsToDirectory) { done(); return }
        active = true; status = "Adding observation and preparing archive…"
        worker.execute {
            record("physical_observation", JSONObject().put("text", text))
            val result = runCatching { archive() }
            main.post { active = false; result.onSuccess { status = "Archive ready.${warningSuffix()}"; done() }.onFailure { status = "Archive creation failed: ${it.message}. ${previousArchiveMessage()}" } }
        }
    }
    fun save(uri: Uri, done: (String) -> Unit) {
        val source = archive ?: return
        active = true; status = "Saving archive…"
        worker.execute {
            val result = runCatching { app.contentResolver.openOutputStream(uri)?.use { out -> source.inputStream().use { it.copyTo(out) } } ?: error("Cannot open destination") }
            main.post { active = false; status = if (result.isSuccess) "Archive saved" else "Save failed: ${result.exceptionOrNull()?.message}"; done(status) }
        }
    }
    private fun finish(message: String) {
        if (!active || finishing) return
        finishing = true
        main.removeCallbacks(timeout); cancelFocusRetry(); grant?.cancel(); grant = null; shizukuBusy = false
        receiver?.let { runCatching { app.unregisterReceiver(it) } }; receiver = null
        permissionReceiver?.let { runCatching { app.unregisterReceiver(it) } }; permissionReceiver = null
        record("finished", JSONObject().put("status", message)); status = "Preparing archive…"
        val runId = generation
        worker.execute {
            val result = runCatching { archive() }
            main.post { if (runId == generation) { active = false; result.onSuccess { status = "$message. Save or share the archive.${warningSuffix()}" }.onFailure { status = "Archive creation failed: ${it.message}. ${previousArchiveMessage()}" } } }
        }
    }
    private fun archive() {
        archiveWarning = null
        if (nativeStarted) runCatching { ValidatorBridge.flush() }.onFailure { error ->
            val warning = "Native debug flush failed: ${error.message}; available evidence may be incomplete."
            archiveWarning = warning
            record("capture_warning", JSONObject().put("stage", "native_debug_flush").put("message", warning))
            runCatching { File(directory, "capture-warning.json").writeText(JSONObject().put("stage", "native_debug_flush").put("message", warning).toString()) }
        }
        val priorWarning = File(directory, "capture-warning.json")
        if (archiveWarning == null && priorWarning.isFile) archiveWarning = runCatching { JSONObject(priorWarning.readText()).optString("message").takeIf { it.isNotBlank() } }.getOrNull()
        val target = File(app.filesDir, "archive-${System.currentTimeMillis()}-${System.nanoTime()}.zip")
        val temporary = File(app.filesDir, target.name + ".tmp")
        val marker = markerFile()
        if (marker.exists()) writeMarker(JSONObject().put("state", "pending").put("published_archive", target.name))
        try {
            ZipOutputStream(temporary.outputStream().buffered()).use { zip ->
                directory.walkTopDown().filter { it.isFile && it.name != "pending.json" && it.name != "pending.json.tmp" && it.name != "capture-warning.json" }.forEach { file ->
                    zip.putNextEntry(ZipEntry(file.relativeTo(directory).invariantSeparatorsPath)); file.inputStream().use { it.copyTo(zip) }; zip.closeEntry()
                }
                val warningFile = File(directory, "capture-warning.json")
                val warning = archiveWarning?.let { JSONObject().put("stage", "native_debug_flush").put("message", it).toString() }
                    ?: warningFile.takeIf { it.isFile }?.readText()
                if (warning != null) { zip.putNextEntry(ZipEntry("capture-warning.json")); zip.write(warning.toByteArray(Charsets.UTF_8)); zip.closeEntry() }
            }
            check(temporary.renameTo(target)) { "Cannot publish archive" }
            archive = target; archiveBelongsToDirectory = true
            if (marker.exists()) marker.delete()
        } finally { temporary.delete() }
    }
}
