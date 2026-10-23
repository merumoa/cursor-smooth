// CursorSmoothTray — portable WinForms companion for cursor-smooth.
//
// Responsibilities: start/stop the engine, show its state, edit the few
// settings that exist, and exit. The engine stays a plain console binary; this
// tray only supervises it through a named stop event, so no engine logic is
// duplicated here.

using System;
using System.Collections.Generic;
using System.Diagnostics;
using System.Drawing;
using System.Drawing.Drawing2D;
using System.IO;
using System.Runtime.InteropServices;
using System.Runtime.Serialization;
using System.Runtime.Serialization.Json;
using System.Text;
using System.Threading;
using System.Windows.Forms;

namespace CursorSmoothTray
{
    [DataContract]
    public sealed class Settings
    {
        [DataMember(Name = "engine_path")]
        public string EnginePath = "";

        [DataMember(Name = "start_on_launch")]
        public bool StartOnLaunch = true;

        [DataMember(Name = "show_notifications")]
        public bool ShowNotifications = true;

        public static string SettingsPath
        {
            get
            {
                string root = Environment.GetFolderPath(Environment.SpecialFolder.ApplicationData);
                return Path.Combine(Path.Combine(root, "cursor-smooth"), "tray.json");
            }
        }

        public static Settings Load()
        {
            try
            {
                string path = SettingsPath;
                if (!File.Exists(path))
                {
                    Settings fresh = new Settings();
                    fresh.EnginePath = DefaultEnginePath();
                    return fresh;
                }
                using (FileStream stream = File.OpenRead(path))
                {
                    DataContractJsonSerializer serializer = new DataContractJsonSerializer(typeof(Settings));
                    Settings loaded = serializer.ReadObject(stream) as Settings;
                    if (loaded == null)
                    {
                        return Fail();
                    }
                    if (string.IsNullOrEmpty(loaded.EnginePath))
                    {
                        loaded.EnginePath = DefaultEnginePath();
                    }
                    return loaded;
                }
            }
            catch (Exception)
            {
                return Fail();
            }
        }

        private static Settings Fail()
        {
            Settings fallback = new Settings();
            fallback.EnginePath = DefaultEnginePath();
            return fallback;
        }

        public void Save()
        {
            string path = SettingsPath;
            Directory.CreateDirectory(Path.GetDirectoryName(path));
            string temporary = path + ".tmp";
            using (FileStream stream = File.Create(temporary))
            {
                DataContractJsonSerializer serializer = new DataContractJsonSerializer(typeof(Settings));
                serializer.WriteObject(stream, this);
            }
            if (File.Exists(path))
            {
                File.Delete(path);
            }
            File.Move(temporary, path);
        }

        /// Looks for the engine relative to the tray binary, then on PATH.
        public static string DefaultEnginePath()
        {
            string baseDir = AppDomain.CurrentDomain.BaseDirectory;
            string[] candidates = new string[]
            {
                Path.GetFullPath(Path.Combine(baseDir, "cursor-smooth.exe")),
                Path.GetFullPath(Path.Combine(baseDir, "..", "cursor-smooth.exe")),
                Path.GetFullPath(Path.Combine(baseDir, "..", "..", "target", "release", "cursor-smooth.exe")),
                Path.GetFullPath(Path.Combine(baseDir, "..", "..", "..", "target", "release", "cursor-smooth.exe")),
            };
            foreach (string candidate in candidates)
            {
                if (File.Exists(candidate))
                {
                    return candidate;
                }
            }
            return candidates[candidates.Length - 1];
        }
    }

    internal static class Log
    {
        private static readonly object Gate = new object();

        public static string Path
        {
            get { return System.IO.Path.Combine(AppDomain.CurrentDomain.BaseDirectory, "tray.log"); }
        }

        public static void Write(string message)
        {
            string line = DateTime.Now.ToString("yyyy-MM-dd HH:mm:ss") + "  " + message + Environment.NewLine;
            try
            {
                lock (Gate)
                {
                    File.AppendAllText(Path, line, new UTF8Encoding(false));
                }
            }
            catch (Exception)
            {
            }
        }
    }

    /// Supervises one engine process. The tray owns the named stop event, so a
    /// stop request is a graceful CLI shutdown instead of a kill.
    internal sealed class EngineController : IDisposable
    {
        private Process process;
        private EventWaitHandle stopEvent;
        private string stopEventName;
        private StringBuilder output = new StringBuilder();

        public event Action<string> Line;
        public event Action<bool, int> StateChanged; // running, exit code

        public bool IsRunning
        {
            get
            {
                try
                {
                    return process != null && !process.HasExited;
                }
                catch (Exception)
                {
                    return false;
                }
            }
        }

        public string RecentOutput
        {
            get
            {
                lock (output)
                {
                    return output.ToString();
                }
            }
        }

        public string Start(string enginePath)
        {
            if (IsRunning)
            {
                return null;
            }
            if (string.IsNullOrEmpty(enginePath) || !File.Exists(enginePath))
            {
                return "engine not found: " + enginePath;
            }
            DisposeEngine();
            stopEventName = "cursor-smooth-tray-" + Guid.NewGuid().ToString("N").Substring(0, 16);
            bool created;
            stopEvent = new EventWaitHandle(false, EventResetMode.AutoReset, stopEventName, out created);
            ProcessStartInfo info = new ProcessStartInfo();
            info.FileName = enginePath;
            info.Arguments = "run --stop-event " + stopEventName;
            info.UseShellExecute = false;
            info.CreateNoWindow = true;
            info.RedirectStandardOutput = true;
            info.RedirectStandardError = true;
            info.StandardOutputEncoding = Encoding.UTF8;
            info.StandardErrorEncoding = Encoding.UTF8;
            info.WorkingDirectory = Path.GetDirectoryName(enginePath);
            Process started = new Process();
            started.StartInfo = info;
            started.EnableRaisingEvents = true;
            started.OutputDataReceived += delegate(object sender, DataReceivedEventArgs args)
            {
                if (args.Data != null)
                {
                    Emit(args.Data);
                }
            };
            started.ErrorDataReceived += delegate(object sender, DataReceivedEventArgs args)
            {
                if (args.Data != null)
                {
                    Emit("stderr: " + args.Data);
                }
            };
            started.Exited += delegate
            {
                int code = -1;
                try
                {
                    code = started.ExitCode;
                }
                catch (Exception)
                {
                }
                Emit("engine exited with code " + code);
                Action<bool, int> handler = StateChanged;
                if (handler != null)
                {
                    handler(false, code);
                }
            };
            try
            {
                started.Start();
            }
            catch (Exception error)
            {
                DisposeEngine();
                return "cannot start engine: " + error.Message;
            }
            started.BeginOutputReadLine();
            started.BeginErrorReadLine();
            process = started;
            Emit("engine started: " + enginePath + " (stop event " + stopEventName + ")");
            Action<bool, int> startedHandler = StateChanged;
            if (startedHandler != null)
            {
                startedHandler(true, -1);
            }
            return null;
        }

        public bool Stop(int graceMilliseconds)
        {
            Process current = process;
            if (current == null)
            {
                return true;
            }
            try
            {
                if (!current.HasExited)
                {
                    if (stopEvent != null)
                    {
                        stopEvent.Set();
                    }
                    if (!current.WaitForExit(graceMilliseconds))
                    {
                        Log.Write("engine did not stop gracefully, killing");
                        current.Kill();
                        current.WaitForExit(2000);
                    }
                }
            }
            catch (Exception error)
            {
                Log.Write("stop failed: " + error.Message);
            }
            bool stopped = !IsRunning;
            DisposeEngine();
            return stopped;
        }

        public string RunDoctor(string enginePath)
        {
            if (string.IsNullOrEmpty(enginePath) || !File.Exists(enginePath))
            {
                return "engine not found: " + enginePath;
            }
            try
            {
                ProcessStartInfo info = new ProcessStartInfo();
                info.FileName = enginePath;
                info.Arguments = "doctor";
                info.UseShellExecute = false;
                info.CreateNoWindow = true;
                info.RedirectStandardOutput = true;
                info.RedirectStandardError = true;
                info.StandardOutputEncoding = Encoding.UTF8;
                info.StandardErrorEncoding = Encoding.UTF8;
                using (Process doctor = Process.Start(info))
                {
                    string standard = doctor.StandardOutput.ReadToEnd();
                    string errors = doctor.StandardError.ReadToEnd();
                    doctor.WaitForExit(10000);
                    return standard + errors;
                }
            }
            catch (Exception error)
            {
                return "doctor failed: " + error.Message;
            }
        }

        private void Emit(string line)
        {
            lock (output)
            {
                output.AppendLine(line);
                if (output.Length > 20000)
                {
                    output.Remove(0, output.Length - 12000);
                }
            }
            Action<string> handler = Line;
            if (handler != null)
            {
                handler(line);
            }
        }

        private void DisposeEngine()
        {
            if (process != null)
            {
                try
                {
                    process.Dispose();
                }
                catch (Exception)
                {
                }
                process = null;
            }
            if (stopEvent != null)
            {
                try
                {
                    stopEvent.Dispose();
                }
                catch (Exception)
                {
                }
                stopEvent = null;
            }
        }

        public void Dispose()
        {
            Stop(3000);
        }
    }

    internal sealed class SettingsForm : Form
    {
        private readonly EngineController engine;
        private Settings settings;
        private bool exitRequested;

        private TextBox pathBox;
        private Label stateLabel;
        private Button startButton;
        private Button stopButton;
        private Button doctorButton;
        private CheckBox startOnLaunchBox;
        private CheckBox notifyBox;
        private TextBox logBox;

        public event Action<Settings> Applied;

        public SettingsForm(EngineController engine, Settings settings)
        {
            this.engine = engine;
            this.settings = settings;
            BuildLayout();
            engine.Line += OnEngineLine;
            engine.StateChanged += OnEngineState;
            UpdateState(engine.IsRunning);
        }

        private void BuildLayout()
        {
            Text = "CursorSmooth — настройки";
            StartPosition = FormStartPosition.CenterScreen;
            FormBorderStyle = FormBorderStyle.FixedDialog;
            MaximizeBox = false;
            MinimizeBox = true;
            ShowInTaskbar = true;
            ClientSize = new Size(560, 430);
            Font = new Font("Segoe UI", 9f);

            GroupBox engineGroup = new GroupBox();
            engineGroup.Text = "Движок";
            engineGroup.SetBounds(12, 12, 536, 128);

            Label pathLabel = new Label();
            pathLabel.Text = "Путь к cursor-smooth.exe:";
            pathLabel.SetBounds(14, 26, 190, 20);
            engineGroup.Controls.Add(pathLabel);

            pathBox = new TextBox();
            pathBox.SetBounds(14, 48, 400, 24);
            pathBox.Text = settings.EnginePath;
            engineGroup.Controls.Add(pathBox);

            Button browse = new Button();
            browse.Text = "Обзор…";
            browse.SetBounds(420, 46, 100, 26);
            browse.Click += OnBrowse;
            engineGroup.Controls.Add(browse);

            stateLabel = new Label();
            stateLabel.SetBounds(14, 84, 300, 20);
            stateLabel.Text = "Состояние: остановлен";
            engineGroup.Controls.Add(stateLabel);

            startButton = new Button();
            startButton.Text = "Запустить";
            startButton.SetBounds(318, 80, 98, 28);
            startButton.Click += OnStart;
            engineGroup.Controls.Add(startButton);

            stopButton = new Button();
            stopButton.Text = "Остановить";
            stopButton.SetBounds(422, 80, 98, 28);
            stopButton.Click += OnStop;
            engineGroup.Controls.Add(stopButton);

            GroupBox tuningGroup = new GroupBox();
            tuningGroup.Text = "Параметры";
            tuningGroup.SetBounds(12, 148, 536, 92);

            startOnLaunchBox = new CheckBox();
            startOnLaunchBox.Text = "Запускать двигатель при старте трея";
            startOnLaunchBox.SetBounds(14, 24, 340, 22);
            startOnLaunchBox.Checked = settings.StartOnLaunch;
            tuningGroup.Controls.Add(startOnLaunchBox);

            notifyBox = new CheckBox();
            notifyBox.Text = "Показывать уведомления";
            notifyBox.SetBounds(14, 52, 340, 22);
            notifyBox.Checked = settings.ShowNotifications;
            tuningGroup.Controls.Add(notifyBox);

            doctorButton = new Button();
            doctorButton.Text = "doctor";
            doctorButton.SetBounds(422, 30, 98, 28);
            doctorButton.Click += OnDoctor;
            tuningGroup.Controls.Add(doctorButton);

            logBox = new TextBox();
            logBox.SetBounds(12, 250, 536, 130);
            logBox.Multiline = true;
            logBox.ReadOnly = true;
            logBox.ScrollBars = ScrollBars.Vertical;
            logBox.BackColor = SystemColors.Window;
            logBox.Font = new Font("Consolas", 8.25f);
            Controls.Add(logBox);

            Button save = new Button();
            save.Text = "Сохранить";
            save.SetBounds(340, 392, 100, 28);
            save.Click += OnSave;
            Controls.Add(save);

            Button close = new Button();
            close.Text = "Закрыть";
            close.SetBounds(448, 392, 100, 28);
            close.Click += delegate { Close(); };
            Controls.Add(close);

            Controls.Add(engineGroup);
            Controls.Add(tuningGroup);
            FormClosing += OnFormClosing;
        }

        private void OnBrowse(object sender, EventArgs args)
        {
            using (OpenFileDialog dialog = new OpenFileDialog())
            {
                dialog.Filter = "cursor-smooth.exe|cursor-smooth.exe|Все файлы|*.*";
                dialog.Title = "Выберите cursor-smooth.exe";
                string current = pathBox.Text.Trim();
                if (current.Length > 0)
                {
                    string directory = Path.GetDirectoryName(current);
                    if (!string.IsNullOrEmpty(directory) && Directory.Exists(directory))
                    {
                        dialog.InitialDirectory = directory;
                    }
                }
                if (dialog.ShowDialog(this) == DialogResult.OK)
                {
                    pathBox.Text = dialog.FileName;
                }
            }
        }

        private void OnStart(object sender, EventArgs args)
        {
            string error = engine.Start(pathBox.Text.Trim());
            if (error != null)
            {
                AppendLog(error);
                MessageBox.Show(this, error, "CursorSmooth", MessageBoxButtons.OK, MessageBoxIcon.Warning);
            }
        }

        private void OnStop(object sender, EventArgs args)
        {
            engine.Stop(3000);
            UpdateState(false);
        }

        private void OnDoctor(object sender, EventArgs args)
        {
            AppendLog("--- doctor ---");
            string report = engine.RunDoctor(pathBox.Text.Trim());
            foreach (string line in report.Split('\n'))
            {
                string trimmed = line.TrimEnd('\r');
                if (trimmed.Length > 0)
                {
                    AppendLog(trimmed);
                }
            }
            AppendLog("--- end doctor ---");
        }

        private void OnSave(object sender, EventArgs args)
        {
            settings.EnginePath = pathBox.Text.Trim();
            settings.StartOnLaunch = startOnLaunchBox.Checked;
            settings.ShowNotifications = notifyBox.Checked;
            try
            {
                settings.Save();
                AppendLog("settings saved to " + Settings.SettingsPath);
            }
            catch (Exception error)
            {
                AppendLog("cannot save settings: " + error.Message);
            }
            Action<Settings> handler = Applied;
            if (handler != null)
            {
                handler(settings);
            }
            Hide();
        }

        private void OnEngineLine(string line)
        {
            if (logBox.IsHandleCreated)
            {
                logBox.BeginInvoke(new Action<string>(AppendLog), line);
            }
        }

        private void AppendLog(string line)
        {
            string stamp = DateTime.Now.ToString("HH:mm:ss");
            logBox.AppendText(stamp + "  " + line + Environment.NewLine);
        }

        public void UpdateState(bool running)
        {
            stateLabel.Text = running ? "Состояние: работает" : "Состояние: остановлен";
            stateLabel.ForeColor = running ? Color.FromArgb(0, 110, 40) : SystemColors.ControlText;
            startButton.Enabled = !running;
            stopButton.Enabled = running;
        }

        private void OnEngineState(bool running, int code)
        {
            if (!IsHandleCreated)
            {
                return;
            }
            BeginInvoke(new Action<bool>(UpdateState), running);
            if (!running && code != 0)
            {
                BeginInvoke(new Action<string>(AppendLog), "engine stopped with code " + code);
            }
        }

        private void OnFormClosing(object sender, FormClosingEventArgs args)
        {
            if (args.CloseReason == CloseReason.UserClosing && !exitRequested)
            {
                args.Cancel = true;
                Hide();
            }
        }

        public void AllowExit()
        {
            exitRequested = true;
        }
    }

    internal sealed class TrayContext : ApplicationContext
    {
        private readonly NotifyIcon icon;
        private readonly EngineController engine = new EngineController();
        private readonly ToolStripMenuItem startItem;
        private readonly ToolStripMenuItem stopItem;
        private readonly ToolStripMenuItem settingsItem;
        private Settings settings;
        private SettingsForm form;
        private readonly EventWaitHandle openSignal;
        private volatile bool alive = true;

        public const string OpenEventName = "Local\\cursor-smooth-tray-open-settings";

        public TrayContext(EventWaitHandle openSignal, bool openSettingsNow, int exitAfterMilliseconds)
        {
            this.openSignal = openSignal;
            settings = Settings.Load();
            Icon trayIcon = LoadIcon();

            ContextMenuStrip menu = new ContextMenuStrip();
            settingsItem = new ToolStripMenuItem("Настройки…", null, delegate { OpenSettings(); });
            startItem = new ToolStripMenuItem("Запустить двигатель", null, delegate { StartEngine(); });
            stopItem = new ToolStripMenuItem("Остановить двигатель", null, delegate { StopEngine(); });
            ToolStripMenuItem exitItem = new ToolStripMenuItem("Выход", null, delegate { ExitApplication(); });
            menu.Items.Add(settingsItem);
            menu.Items.Add(new ToolStripSeparator());
            menu.Items.Add(startItem);
            menu.Items.Add(stopItem);
            menu.Items.Add(new ToolStripSeparator());
            menu.Items.Add(exitItem);

            icon = new NotifyIcon();
            icon.Icon = trayIcon;
            icon.Text = "CursorSmooth";
            icon.ContextMenuStrip = menu;
            icon.Visible = true;
            icon.DoubleClick += delegate { OpenSettings(); };

            engine.Line += delegate(string line) { Log.Write(line); };
            engine.StateChanged += OnEngineState;
            UpdateMenu(false);

            form = new SettingsForm(engine, settings);
            form.Applied += delegate(Settings applied)
            {
                settings = applied;
                form.UpdateState(engine.IsRunning);
            };
            IntPtr unused = form.Handle; // create the window handle on the UI thread

            if (settings.StartOnLaunch)
            {
                StartEngine();
            }

            Thread waiter = new Thread(WatchOpenRequests);
            waiter.IsBackground = true;
            waiter.Name = "tray-open-requests";
            waiter.Start();

            if (openSettingsNow)
            {
                OpenSettings();
            }

            if (exitAfterMilliseconds > 0)
            {
                // Test hook: proves the exit path (engine stop + icon teardown)
                // without needing a human to click the tray menu.
                System.Windows.Forms.Timer exitTimer = new System.Windows.Forms.Timer();
                exitTimer.Interval = exitAfterMilliseconds;
                exitTimer.Tick += delegate
                {
                    exitTimer.Stop();
                    exitTimer.Dispose();
                    ExitApplication();
                };
                exitTimer.Start();
            }
        }

        /// A second launch signals this event instead of starting a rival tray;
        /// the desktop shortcut therefore always lands on the settings window.
        private void WatchOpenRequests()
        {
            while (alive)
            {
                try
                {
                    openSignal.WaitOne();
                }
                catch (Exception)
                {
                    return;
                }
                if (!alive)
                {
                    return;
                }
                try
                {
                    form.BeginInvoke(new Action(OpenSettings));
                }
                catch (Exception error)
                {
                    Log.Write("open request failed: " + error.Message);
                }
            }
        }

        private static Icon LoadIcon()
        {
            try
            {
                Icon extracted = Icon.ExtractAssociatedIcon(Application.ExecutablePath);
                if (extracted != null)
                {
                    return extracted;
                }
            }
            catch (Exception)
            {
            }
            return SystemIcons.Application;
        }

        private void StartEngine()
        {
            string error = engine.Start(settings.EnginePath);
            if (error != null)
            {
                Log.Write(error);
                if (settings.ShowNotifications)
                {
                    icon.ShowBalloonTip(4000, "CursorSmooth", error, ToolTipIcon.Warning);
                }
            }
            UpdateMenu(engine.IsRunning);
        }

        private void StopEngine()
        {
            engine.Stop(3000);
            UpdateMenu(false);
        }

        private void OnEngineState(bool running, int code)
        {
            UpdateMenu(running);
            if (!running && code != 0 && settings.ShowNotifications)
            {
                icon.ShowBalloonTip(4000, "CursorSmooth", "Движок завершился с кодом " + code, ToolTipIcon.Warning);
            }
            if (form != null && form.IsHandleCreated)
            {
                form.BeginInvoke(new Action<bool>(form.UpdateState), running);
            }
        }

        private void UpdateMenu(bool running)
        {
            startItem.Enabled = !running;
            stopItem.Enabled = running;
            icon.Text = running ? "CursorSmooth — работает" : "CursorSmooth — остановлен";
        }

        private void OpenSettings()
        {
            form.Show();
            form.WindowState = FormWindowState.Normal;
            form.Activate();
        }

        private void ExitApplication()
        {
            alive = false;
            try
            {
                openSignal.Set();
            }
            catch (Exception)
            {
            }
            try
            {
                engine.Stop(3000);
            }
            catch (Exception error)
            {
                Log.Write("exit stop failed: " + error.Message);
            }
            icon.Visible = false;
            icon.Dispose();
            form.AllowExit();
            form.Close();
            ExitThread();
        }
    }

    internal static class Program
    {
        [STAThread]
        private static void Main(string[] args)
        {
            Application.EnableVisualStyles();
            Application.SetCompatibleTextRenderingDefault(false);

            Application.ThreadException += delegate(object sender, System.Threading.ThreadExceptionEventArgs failure)
            {
                Log.Write("thread exception: " + failure.Exception);
                MessageBox.Show(failure.Exception.Message, "CursorSmooth", MessageBoxButtons.OK, MessageBoxIcon.Error);
            };
            AppDomain.CurrentDomain.UnhandledException += delegate(object sender, UnhandledExceptionEventArgs failure)
            {
                Log.Write("unhandled exception: " + failure.ExceptionObject);
            };

            if (args.Length > 0 && args[0] == "--selftest")
            {
                Environment.Exit(SelfTest.Run());
                return;
            }

            bool openSettings = Array.IndexOf(args, "--settings") >= 0;
            int exitAfter = 0;
            int exitIndex = Array.IndexOf(args, "--exit-after-ms");
            if (exitIndex >= 0 && exitIndex + 1 < args.Length)
            {
                int parsed;
                if (int.TryParse(args[exitIndex + 1], out parsed) && parsed > 0)
                {
                    exitAfter = Math.Min(parsed, 600000);
                }
            }
            bool firstInstance;
            EventWaitHandle openSignal = new EventWaitHandle(
                false,
                EventResetMode.AutoReset,
                TrayContext.OpenEventName,
                out firstInstance);
            if (!firstInstance)
            {
                // Another tray owns the icon: ask it to show settings, then leave.
                openSignal.Set();
                openSignal.Dispose();
                return;
            }
            using (openSignal)
            {
                Application.Run(new TrayContext(openSignal, openSettings, exitAfter));
            }
        }
    }

    internal static class SelfTest
    {
        public static int Run()
        {
            int failures = 0;
            failures += Check("settings round-trip", SettingsRoundTrip);
            failures += Check("default engine path", DefaultEnginePath);
            failures += Check("engine arguments", EngineArguments);
            failures += Check("icon available", IconAvailable);
            failures += Check("log writable", LogWritable);
            Console.WriteLine(failures == 0 ? "selftest: ok" : "selftest: " + failures + " failure(s)");
            return failures == 0 ? 0 : 1;
        }

        private static int Check(string name, Func<string> probe)
        {
            try
            {
                string error = probe();
                if (error == null)
                {
                    Console.WriteLine("pass  " + name);
                    return 0;
                }
                Console.WriteLine("FAIL  " + name + ": " + error);
                return 1;
            }
            catch (Exception error)
            {
                Console.WriteLine("FAIL  " + name + ": " + error.Message);
                return 1;
            }
        }

        private static string SettingsRoundTrip()
        {
            Settings original = new Settings();
            original.EnginePath = @"C:\tmp\cursor-smooth.exe";
            original.StartOnLaunch = false;
            original.ShowNotifications = false;
            string path = Path.Combine(Path.GetTempPath(), "cursor-smooth-selftest.json");
            using (FileStream stream = File.Create(path))
            {
                DataContractJsonSerializer serializer = new DataContractJsonSerializer(typeof(Settings));
                serializer.WriteObject(stream, original);
            }
            Settings restored;
            using (FileStream stream = File.OpenRead(path))
            {
                DataContractJsonSerializer serializer = new DataContractJsonSerializer(typeof(Settings));
                restored = serializer.ReadObject(stream) as Settings;
            }
            File.Delete(path);
            if (restored == null)
            {
                return "deserialised null";
            }
            if (restored.EnginePath != original.EnginePath)
            {
                return "path mismatch";
            }
            if (restored.StartOnLaunch != original.StartOnLaunch || restored.ShowNotifications != original.ShowNotifications)
            {
                return "flags mismatch";
            }
            return null;
        }

        private static string DefaultEnginePath()
        {
            string resolved = Settings.DefaultEnginePath();
            if (string.IsNullOrEmpty(resolved))
            {
                return "empty path";
            }
            if (!resolved.EndsWith("cursor-smooth.exe", StringComparison.OrdinalIgnoreCase))
            {
                return "unexpected name: " + resolved;
            }
            return null;
        }

        private static string EngineArguments()
        {
            string name = "cursor-smooth-tray-" + Guid.NewGuid().ToString("N").Substring(0, 16);
            string arguments = "run --stop-event " + name;
            if (!arguments.StartsWith("run --stop-event cursor-smooth-tray-", StringComparison.Ordinal))
            {
                return "unexpected arguments: " + arguments;
            }
            if (name.Length != "cursor-smooth-tray-".Length + 16)
            {
                return "event name length " + name.Length;
            }
            return null;
        }

        private static string IconAvailable()
        {
            using (Bitmap bitmap = new Bitmap(32, 32))
            {
                using (Graphics graphics = Graphics.FromImage(bitmap))
                {
                    graphics.Clear(Color.Transparent);
                    using (SolidBrush brush = new SolidBrush(Color.White))
                    {
                        graphics.FillEllipse(brush, 4, 4, 24, 24);
                    }
                }
                IntPtr handle = bitmap.GetHicon();
                try
                {
                    using (Icon icon = Icon.FromHandle(handle))
                    {
                        if (icon.Width <= 0)
                        {
                            return "zero width";
                        }
                    }
                }
                finally
                {
                    DestroyIcon(handle);
                }
            }
            return null;
        }

        private static string LogWritable()
        {
            Log.Write("selftest");
            return File.Exists(Log.Path) ? null : "log file missing";
        }

        [DllImport("user32.dll", SetLastError = true)]
        private static extern bool DestroyIcon(IntPtr handle);
    }
}
