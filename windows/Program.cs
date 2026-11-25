using System;
using System.IO;
using System.Linq;
using System.Runtime.InteropServices;
using System.Text;
using System.Text.Json;
using System.Text.Json.Serialization;
using System.Threading;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Controls;
using System.Windows.Threading;
using Microsoft.Win32;

internal static class Program
{
    [STAThread]
    private static void Main() => new Application().Run(new SyncWindow());
}

internal sealed class SyncWindow : Window
{
    private readonly TextBlock configLabel = new() { TextWrapping = TextWrapping.Wrap };
    private readonly TextBlock destinationLabel = new() { TextWrapping = TextWrapping.Wrap };
    private readonly TextBlock result = new() { TextWrapping = TextWrapping.Wrap, Margin = new Thickness(0, 16, 0, 0) };
    private readonly TextBlock rtcRouteStatus = new() { TextWrapping = TextWrapping.Wrap, Margin = new Thickness(0, 4, 0, 0) };
    private readonly Button configButton = new() { Content = "設定ファイルを選択" };
    private readonly Button destinationButton = new() { Content = "保存先を選択" };
    private readonly Button syncButton = new() { Content = "同期を開始", IsEnabled = false };
    private readonly Button cancelButton = new() { Content = "同期を中断", IsEnabled = false, Margin = new Thickness(0, 8, 0, 0), HorizontalAlignment = HorizontalAlignment.Left };
    private CancellationTokenSource? cancellation;
    private readonly ListBox originalList = new() { MaxHeight = 240 };
    private readonly Expander originals = new() { Header = "保護原本の一覧", Visibility = Visibility.Collapsed };
    private readonly ListBox conflictList = new() { MaxHeight = 240 };
    private readonly Expander conflicts = new() { Header = "未解決の競合", Visibility = Visibility.Collapsed };
    private readonly ListBox historyList = new() { MaxHeight = 240 };
    private readonly Expander history = new() { Header = "48時間の履歴", Visibility = Visibility.Collapsed };
    private readonly Button writerConfigButton = new() { Content = "書き込みピア設定を選択" };
    private readonly TextBlock writerConfigLabel = new() { TextWrapping = TextWrapping.Wrap };
    private readonly Button writerStateButton = new() { Content = "書き込みピアの状態フォルダーを選択" };
    private readonly TextBlock writerStateLabel = new() { TextWrapping = TextWrapping.Wrap };
    private readonly Button writerSourceButton = new() { Content = "共有元フォルダーを選択" };
    private readonly TextBlock writerSourceLabel = new() { TextWrapping = TextWrapping.Wrap };
    private readonly Button refreshWriterButton = new() { Content = "共有元を登録・更新", Margin = new Thickness(0, 12, 0, 0) };
    private readonly Button loadMissingButton = new() { Content = "共有元から消えたファイルを確認", Margin = new Thickness(0, 12, 0, 0) };
    private readonly ListBox missingList = new() { MaxHeight = 200 };
    private readonly Button deleteMissingButton = new() { Content = "選択したファイルの削除を同期", Margin = new Thickness(0, 8, 0, 0) };
    private readonly Button relocateMissingButton = new() { Content = "選択したファイルの移動先を指定", Margin = new Thickness(0, 8, 0, 0) };
    private readonly TextBlock missingStatus = new() { TextWrapping = TextWrapping.Wrap };
    private readonly Button loadWriterProtectionButton = new() { Content = "保護原本と容量を確認", Margin = new Thickness(0, 12, 0, 0) };
    private readonly ListBox writerOriginalList = new() { MaxHeight = 240 };
    private readonly TextBlock writerProtectionStatus = new() { TextWrapping = TextWrapping.Wrap };
    private readonly Button catchUpWriterButton = new() { Content = "他のピアから取得", Margin = new Thickness(0, 12, 0, 0) };
    private readonly Button cancelCatchUpButton = new() { Content = "取得を中断", Margin = new Thickness(0, 8, 0, 0) };
    private readonly Button loadWriterHistoryButton = new() { Content = "復元できる履歴を確認", Margin = new Thickness(0, 12, 0, 0) };
    private readonly ListBox writerHistoryList = new() { MaxHeight = 240 };
    private readonly Button restoreHistoryButton = new() { Content = "選択した履歴を復元", Margin = new Thickness(0, 8, 0, 0) };
    private readonly Button deleteHistoryButton = new() { Content = "選択した履歴を削除", Margin = new Thickness(0, 8, 0, 0) };
    private readonly Button deleteAllHistoryButton = new() { Content = "表示中の履歴をすべて削除", Margin = new Thickness(0, 8, 0, 0) };
    private readonly Button loadWriterConflictsButton = new() { Content = "競合を確認", Margin = new Thickness(0, 16, 0, 0) };
    private readonly ListBox writerConflictList = new() { MaxHeight = 180 };
    private readonly ListBox writerCandidateList = new() { MaxHeight = 180 };
    private readonly Button resolveConflictButton = new() { Content = "選択した版を採用", Margin = new Thickness(0, 8, 0, 0) };
    private readonly Button startServerButton = new() { Content = "配信を開始", Margin = new Thickness(0, 12, 0, 0) };
    private readonly Button stopServerButton = new() { Content = "配信を停止", Margin = new Thickness(0, 8, 0, 0) };
    private readonly TextBlock serverStatus = new() { Text = "配信停止中", TextWrapping = TextWrapping.Wrap };
    private readonly TextBlock writerStatus = new() { TextWrapping = TextWrapping.Wrap, Margin = new Thickness(0, 8, 0, 0) };
    private readonly TextBlock historyStatus = new() { TextWrapping = TextWrapping.Wrap, Margin = new Thickness(0, 8, 0, 0) };
    private readonly TextBlock conflictStatus = new() { TextWrapping = TextWrapping.Wrap, Margin = new Thickness(0, 8, 0, 0) };
    private readonly CheckBox autoSync = new()
    {
        Content = "10 秒ごとに自動同期",
        Margin = new Thickness(0, 12, 0, 0),
        IsEnabled = false,
    };
    private readonly DispatcherTimer syncTimer = new() { Interval = TimeSpan.FromSeconds(10) };
    private readonly System.Windows.Forms.NotifyIcon tray = new();
    private readonly System.Windows.Forms.ToolStripMenuItem traySync = new("同期を開始");
    private readonly System.Windows.Forms.ToolStripMenuItem trayStartServer = new("配信を開始");
    private readonly System.Windows.Forms.ToolStripMenuItem trayStopServer = new("配信を停止");
    private bool syncing;
    private bool writerServing;
    private CancellationTokenSource? serverCancellation;
    private CancellationTokenSource? catchUpCancellation;
    private bool closing;
    private string? configuration;
    private string? destination;
    private string? writerConfiguration;
    private string? writerState;
    private string? writerSource;

    public SyncWindow()
    {
        Title = "P2P Sync";
        Width = 560;
        MinWidth = 420;
        Height = 760;
        MinHeight = 440;
        var panel = new StackPanel { Margin = new Thickness(24) };
        foreach (var action in new[] { refreshWriterButton, loadMissingButton, deleteMissingButton, relocateMissingButton, loadWriterProtectionButton,
            catchUpWriterButton, cancelCatchUpButton,
            startServerButton, stopServerButton,
            loadWriterHistoryButton, restoreHistoryButton, deleteHistoryButton,
            deleteAllHistoryButton, loadWriterConflictsButton, resolveConflictButton })
        {
            action.HorizontalAlignment = HorizontalAlignment.Left;
        }
        panel.Children.Add(new TextBlock { Text = "直接同期", FontSize = 24, FontWeight = FontWeights.SemiBold });
        AddPicker(panel, configButton, configLabel, "接続設定が未選択です");
        AddPicker(panel, destinationButton, destinationLabel, "保存先が未選択です");
        syncButton.Margin = new Thickness(0, 16, 0, 0);
        syncButton.HorizontalAlignment = HorizontalAlignment.Left;
        panel.Children.Add(syncButton);
        panel.Children.Add(cancelButton);
        panel.Children.Add(autoSync);
        panel.Children.Add(result);
        panel.Children.Add(rtcRouteStatus);
        originals.Content = originalList;
        panel.Children.Add(originals);
        conflicts.Content = conflictList;
        panel.Children.Add(conflicts);
        history.Content = historyList;
        panel.Children.Add(history);
        panel.Children.Add(new TextBlock { Text = "共有・配信・履歴", FontSize = 18, Margin = new Thickness(0, 20, 0, 0) });
        AddPicker(panel, writerConfigButton, writerConfigLabel, "書き込みピア設定が未選択です");
        AddPicker(panel, writerStateButton, writerStateLabel, "状態フォルダーが未選択です");
        AddPicker(panel, writerSourceButton, writerSourceLabel, "共有元フォルダーが未選択です");
        panel.Children.Add(refreshWriterButton);
        panel.Children.Add(catchUpWriterButton);
        panel.Children.Add(cancelCatchUpButton);
        panel.Children.Add(startServerButton);
        panel.Children.Add(stopServerButton);
        panel.Children.Add(serverStatus);
        panel.Children.Add(writerStatus);
        panel.Children.Add(loadMissingButton);
        panel.Children.Add(missingList);
        panel.Children.Add(deleteMissingButton);
        panel.Children.Add(relocateMissingButton);
        panel.Children.Add(missingStatus);
        panel.Children.Add(new TextBlock { Text = "保護中の共通原本", FontSize = 18, Margin = new Thickness(0, 20, 0, 0) });
        panel.Children.Add(loadWriterProtectionButton);
        panel.Children.Add(writerProtectionStatus);
        panel.Children.Add(writerOriginalList);
        panel.Children.Add(new TextBlock { Text = "履歴", FontSize = 18, Margin = new Thickness(0, 20, 0, 0) });
        panel.Children.Add(loadWriterHistoryButton);
        panel.Children.Add(writerHistoryList);
        panel.Children.Add(restoreHistoryButton);
        panel.Children.Add(deleteHistoryButton);
        panel.Children.Add(deleteAllHistoryButton);
        panel.Children.Add(historyStatus);
        panel.Children.Add(new TextBlock { Text = "競合の解決", FontSize = 18, Margin = new Thickness(0, 20, 0, 0) });
        panel.Children.Add(loadWriterConflictsButton);
        panel.Children.Add(writerConflictList);
        panel.Children.Add(new TextBlock { Text = "採用する版", Margin = new Thickness(0, 8, 0, 0) });
        panel.Children.Add(writerCandidateList);
        panel.Children.Add(resolveConflictButton);
        panel.Children.Add(conflictStatus);
        Content = new ScrollViewer { Content = panel, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
        RestorePreferences();
        var trayMenu = new System.Windows.Forms.ContextMenuStrip();
        trayMenu.Items.Add("画面を開く", null, (_, _) => ShowWindow());
        trayMenu.Items.Add(traySync);
        trayMenu.Items.Add(trayStartServer);
        trayMenu.Items.Add(trayStopServer);
        trayMenu.Items.Add(new System.Windows.Forms.ToolStripSeparator());
        trayMenu.Items.Add("終了", null, (_, _) => Close());
        tray.ContextMenuStrip = trayMenu;
        tray.Icon = System.Drawing.SystemIcons.Application;
        tray.Text = "P2P Sync";
        tray.Visible = true;
        tray.DoubleClick += (_, _) => ShowWindow();
        traySync.Click += async (_, _) => await RunSync();
        trayStartServer.Click += async (_, _) => await StartServer();
        trayStopServer.Click += (_, _) => StopServer();
        StateChanged += (_, _) =>
        {
            if (WindowState == WindowState.Minimized)
            {
                Hide();
            }
        };

        configButton.Click += (_, _) => ChoosePath(() =>
        {
            var picker = new OpenFileDialog { Filter = "接続設定 (*.json)|*.json", CheckFileExists = true };
            if (picker.ShowDialog(this) == true)
            {
                configuration = picker.FileName;
                configLabel.Text = configuration;
            }
        });
        destinationButton.Click += (_, _) => ChoosePath(() =>
        {
            var picker = new OpenFolderDialog { Title = "同期先フォルダー" };
            if (picker.ShowDialog(this) == true)
            {
                destination = picker.FolderName;
                destinationLabel.Text = destination;
            }
        });
        writerConfigButton.Click += (_, _) => ChoosePath(() =>
        {
            var picker = new OpenFileDialog { Filter = "配信設定 (*.json)|*.json", CheckFileExists = true };
            if (picker.ShowDialog(this) == true)
            {
                writerConfiguration = picker.FileName;
                writerConfigLabel.Text = writerConfiguration;
                writerHistoryList.ItemsSource = null;
                writerConflictList.ItemsSource = null;
                writerCandidateList.ItemsSource = null;
                writerStatus.Text = historyStatus.Text = conflictStatus.Text = "";
                ClearWriterProtection();
                SavePreferences();
            }
        });
        writerStateButton.Click += (_, _) => ChoosePath(() =>
        {
            var picker = new OpenFolderDialog { Title = "書き込みピアの状態フォルダー" };
            if (picker.ShowDialog(this) == true)
            {
                writerState = picker.FolderName;
                writerStateLabel.Text = writerState;
                writerHistoryList.ItemsSource = null;
                writerConflictList.ItemsSource = null;
                writerCandidateList.ItemsSource = null;
                writerStatus.Text = historyStatus.Text = conflictStatus.Text = "";
                ClearWriterProtection();
                SavePreferences();
            }
        });
        writerSourceButton.Click += (_, _) => ChoosePath(() =>
        {
            var picker = new OpenFolderDialog { Title = "共有元フォルダー" };
            if (picker.ShowDialog(this) == true)
            {
                writerSource = picker.FolderName;
                writerSourceLabel.Text = writerSource;
                writerStatus.Text = "";
                missingList.ItemsSource = null;
                missingStatus.Text = "";
                SavePreferences();
            }
        });
        refreshWriterButton.Click += async (_, _) => await RefreshWriterTree();
        loadMissingButton.Click += async (_, _) => await LoadMissingFiles();
        deleteMissingButton.Click += async (_, _) => await DeleteMissingFile();
        relocateMissingButton.Click += async (_, _) => await RelocateMissingFile();
        missingList.SelectionChanged += (_, _) => UpdateControls();
        loadWriterProtectionButton.Click += async (_, _) => await LoadWriterProtection();
        catchUpWriterButton.Click += async (_, _) => await CatchUpWriter();
        cancelCatchUpButton.Click += (_, _) =>
        {
            catchUpCancellation?.Cancel();
            writerStatus.Text = "取得を中断しています…";
            UpdateControls();
        };
        loadWriterHistoryButton.Click += async (_, _) => await LoadWriterHistory();
        writerHistoryList.SelectionChanged += (_, _) => UpdateControls();
        restoreHistoryButton.Click += async (_, _) => await RestoreWriterHistory();
        deleteHistoryButton.Click += async (_, _) => await DeleteWriterHistory(false);
        deleteAllHistoryButton.Click += async (_, _) => await DeleteWriterHistory(true);
        loadWriterConflictsButton.Click += async (_, _) => await LoadWriterConflicts();
        writerConflictList.SelectionChanged += (_, _) =>
        {
            writerCandidateList.ItemsSource = (writerConflictList.SelectedItem as SyncCore.WriterConflict)?.Versions;
            UpdateControls();
        };
        writerCandidateList.SelectionChanged += (_, _) => UpdateControls();
        resolveConflictButton.Click += async (_, _) => await ResolveWriterConflict();
        startServerButton.Click += async (_, _) => await StartServer();
        stopServerButton.Click += (_, _) => StopServer();
        syncButton.Click += async (_, _) => await RunSync();
        cancelButton.Click += (_, _) =>
        {
            autoSync.IsChecked = false;
            cancellation?.Cancel();
            cancelButton.IsEnabled = false;
            result.Text = "同期を中断しています…";
        };
        autoSync.Checked += async (_, _) =>
        {
            SavePreferences();
            await RunSync();
        };
        autoSync.Unchecked += (_, _) =>
        {
            syncTimer.Stop();
            SavePreferences();
        };
        syncTimer.Tick += async (_, _) => await RunSync();
        Closed += (_, _) =>
        {
            closing = true;
            syncTimer.Stop();
            cancellation?.Cancel();
            catchUpCancellation?.Cancel();
            serverCancellation?.Cancel();
            tray.Visible = false;
            tray.Dispose();
            trayMenu.Dispose();
        };
        Loaded += async (_, _) =>
        {
            UpdateControls();
            if (autoSync.IsChecked == true)
            {
                await RunSync();
            }
        };
    }

    private void ShowWindow()
    {
        Show();
        WindowState = WindowState.Normal;
        Activate();
    }

    private void ChoosePath(Action choose)
    {
        syncTimer.Stop();
        try
        {
            var previous = (configuration, destination);
            choose();
            if (previous != (configuration, destination))
            {
                result.Text = "";
                originalList.ItemsSource = null;
                originals.Visibility = Visibility.Collapsed;
                conflictList.ItemsSource = null;
                conflicts.Visibility = Visibility.Collapsed;
                historyList.ItemsSource = null;
                history.Visibility = Visibility.Collapsed;
                SavePreferences();
            }
        }
        finally
        {
            UpdateControls();
        }
    }

    private sealed record Preferences(string? Configuration, string? Destination, bool AutomaticSync,
        string? WriterConfiguration, string? WriterState, string? WriterSource);

    private static string PreferencesPath => Path.Combine(
        Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "P2PSync", "preferences.json");

    private void RestorePreferences()
    {
        try
        {
            if (!File.Exists(PreferencesPath))
            {
                return;
            }
            var preferences = JsonSerializer.Deserialize<Preferences>(File.ReadAllText(PreferencesPath));
            if (preferences is null)
            {
                return;
            }
            configuration = File.Exists(preferences.Configuration) ? preferences.Configuration : null;
            destination = Directory.Exists(preferences.Destination) ? preferences.Destination : null;
            writerConfiguration = File.Exists(preferences.WriterConfiguration) ? preferences.WriterConfiguration : null;
            writerState = Directory.Exists(preferences.WriterState) ? preferences.WriterState : null;
            writerSource = Directory.Exists(preferences.WriterSource) ? preferences.WriterSource : null;
            configLabel.Text = configuration ?? "接続設定をもう一度選択してください";
            destinationLabel.Text = destination ?? "保存先をもう一度選択してください";
            writerConfigLabel.Text = writerConfiguration ?? "書き込みピア設定を選択してください";
            writerStateLabel.Text = writerState ?? "書き込みピアの状態フォルダーを選択してください";
            writerSourceLabel.Text = writerSource ?? "共有元フォルダーを選択してください";
            autoSync.IsChecked = preferences.AutomaticSync;
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException or JsonException)
        {
            result.Text = "保存した設定を読み込めませんでした。設定ファイルと保存先を選択してください。";
        }
    }

    private void SavePreferences()
    {
        string temporary = PreferencesPath + "." + Guid.NewGuid().ToString("N") + ".tmp";
        try
        {
            Directory.CreateDirectory(Path.GetDirectoryName(PreferencesPath)!);
            var preferences = new Preferences(configuration, destination, autoSync.IsChecked == true,
                writerConfiguration, writerState, writerSource);
            using (var file = new FileStream(temporary, FileMode.CreateNew, FileAccess.Write, FileShare.None))
            {
                JsonSerializer.Serialize(file, preferences);
                file.Flush(flushToDisk: true);
            }
            File.Move(temporary, PreferencesPath, overwrite: true);
        }
        catch (Exception error) when (error is IOException or UnauthorizedAccessException)
        {
            result.Text = "設定を保存できませんでした。";
        }
        finally
        {
            try
            {
                File.Delete(temporary);
            }
            catch (Exception error) when (error is IOException or UnauthorizedAccessException)
            {
                // A failed cleanup must not interrupt synchronization.
            }
        }
    }

    private void UpdateControls()
    {
        if (closing)
        {
            return;
        }
        bool ready = configuration is not null && destination is not null;
        configButton.IsEnabled = destinationButton.IsEnabled = !syncing;
        writerConfigButton.IsEnabled = writerStateButton.IsEnabled = !syncing && !writerServing;
        writerSourceButton.IsEnabled = !syncing && !writerServing;
        refreshWriterButton.IsEnabled = !syncing && !writerServing && writerConfiguration is not null
            && writerState is not null && writerSource is not null;
        loadMissingButton.IsEnabled = refreshWriterButton.IsEnabled;
        deleteMissingButton.IsEnabled = loadMissingButton.IsEnabled
            && missingList.SelectedItem is SyncCore.MissingSourceFile;
        relocateMissingButton.IsEnabled = deleteMissingButton.IsEnabled;
        loadWriterProtectionButton.IsEnabled = !syncing && !writerServing && writerConfiguration is not null
            && writerState is not null;
        catchUpWriterButton.IsEnabled = !syncing && !writerServing && writerConfiguration is not null
            && writerState is not null;
        cancelCatchUpButton.IsEnabled = catchUpCancellation?.IsCancellationRequested == false;
        startServerButton.IsEnabled = !syncing && !writerServing && writerConfiguration is not null && writerState is not null;
        stopServerButton.IsEnabled = writerServing && serverCancellation?.IsCancellationRequested == false;
        trayStartServer.Enabled = startServerButton.IsEnabled;
        trayStopServer.Enabled = stopServerButton.IsEnabled;
        loadWriterHistoryButton.IsEnabled = !syncing && !writerServing && writerConfiguration is not null && writerState is not null;
        loadWriterConflictsButton.IsEnabled = loadWriterHistoryButton.IsEnabled;
        restoreHistoryButton.IsEnabled = loadWriterHistoryButton.IsEnabled
            && writerHistoryList.SelectedItem is SyncCore.WriterHistoryEntry { CurrentHeads.Length: 1 };
        deleteHistoryButton.IsEnabled = loadWriterHistoryButton.IsEnabled
            && writerHistoryList.SelectedItem is SyncCore.WriterHistoryEntry;
        deleteAllHistoryButton.IsEnabled = loadWriterHistoryButton.IsEnabled
            && writerHistoryList.ItemsSource is SyncCore.WriterHistoryEntry[] { Length: > 0 };
        resolveConflictButton.IsEnabled = loadWriterConflictsButton.IsEnabled
            && writerConflictList.SelectedItem is SyncCore.WriterConflict { Versions.Length: >= 2 } conflict
            && !conflict.Versions.Any(version => version.Kind == "directory")
            && writerCandidateList.SelectedItem is SyncCore.WriterConflictVersion;
        syncButton.IsEnabled = ready && !syncing;
        traySync.Enabled = ready && !syncing;
        tray.Text = syncing ? "P2P Sync - 同期中" : writerServing ? "P2P Sync - 配信中" : "P2P Sync";
        cancelButton.IsEnabled = syncing && cancellation?.IsCancellationRequested == false;
        autoSync.IsEnabled = ready;
        if (ready && !syncing && autoSync.IsChecked == true)
        {
            syncTimer.Start();
        }
        else
        {
            syncTimer.Stop();
        }
    }

    private void ClearWriterProtection()
    {
        writerOriginalList.ItemsSource = null;
        writerProtectionStatus.Text = "";
        missingList.ItemsSource = null;
        missingStatus.Text = "";
    }

    private static void AddPicker(StackPanel panel, Button button, TextBlock label, string placeholder)
    {
        button.Margin = new Thickness(0, 16, 0, 4);
        button.HorizontalAlignment = HorizontalAlignment.Left;
        label.Text = placeholder;
        panel.Children.Add(button);
        panel.Children.Add(label);
    }

    private async Task StartServer()
    {
        if (closing || syncing || writerServing || writerConfiguration is not string config || writerState is not string state)
        {
            return;
        }
        using var operation = new CancellationTokenSource();
        serverCancellation = operation;
        writerServing = true;
        serverStatus.Text = "配信を開始しています…";
        UpdateControls();
        try
        {
            var rtc = Path.Combine(Path.GetDirectoryName(config)!, "rtc.json");
            var bridgeStarted = 0;
            var status = await Task.Run(() => SyncCore.ServeWriter(config, state, operation.Token, address =>
            {
                Dispatcher.BeginInvoke(new Action(() =>
                {
                    if (!closing) serverStatus.Text = $"配信中: {address}";
                }));
                if (File.Exists(rtc) && Interlocked.Exchange(ref bridgeStarted, 1) == 0)
                {
                    _ = Task.Run(() => SyncCore.RunRtcBridge(rtc, operation.Token, phase =>
                        Dispatcher.BeginInvoke(new Action(() =>
                        {
                            if (!closing && writerServing) serverStatus.Text = $"配信中: {address} · {SyncCore.RtcStatusText(phase)}";
                        }))));
                }
            }));
            if (!closing)
            {
                serverStatus.Text = status == 7 ? "配信を停止しました。" :
                    $"配信を終了しました。{SyncCore.ErrorMessage(status)}";
            }
        }
        catch (Exception error) when (error is DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            serverStatus.Text = "配信処理を読み込めませんでした。";
        }
        finally
        {
            operation.Cancel();
            writerServing = false;
            serverCancellation = null;
            UpdateControls();
        }
    }

    private void StopServer()
    {
        serverCancellation?.Cancel();
        serverStatus.Text = "配信を停止しています…";
        UpdateControls();
    }

    private async Task RefreshWriterTree()
    {
        if (closing || syncing || writerServing || writerConfiguration is not string config
            || writerState is not string state || writerSource is not string source)
        {
            return;
        }
        syncing = true;
        writerStatus.Text = "共有元を確認しています…";
        UpdateControls();
        try
        {
            var scan = await Task.Run(() => SyncCore.RefreshWriterTree(config, state, source));
            writerStatus.Text = scan.Status == 0
                ? $"共有元を登録・更新しました。ファイル {scan.Summary.Files} 件、フォルダー {scan.Summary.Directories} 件、対象外 {scan.Summary.Skipped} 件。見つからないファイルは削除しません。"
                : scan.Status == 2 ? "共有元に設定ファイルや状態フォルダーが含まれています。別のフォルダーを選択してください。"
                : SyncCore.ErrorMessage(scan.Status);
            if (scan.Status == 0)
            {
                ClearWriterProtection();
                writerHistoryList.ItemsSource = null;
                writerConflictList.ItemsSource = null;
                writerCandidateList.ItemsSource = null;
                historyStatus.Text = conflictStatus.Text = "";
            }
        }
        catch (Exception error) when (error is JsonException or DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            writerStatus.Text = "共有元の登録・更新処理を読み込めませんでした。";
        }
        finally
        {
            syncing = false;
            UpdateControls();
        }
    }

    private async Task LoadMissingFiles()
    {
        if (closing || syncing || writerServing || writerConfiguration is not string config
            || writerState is not string state || writerSource is not string source)
        {
            return;
        }
        syncing = true;
        missingStatus.Text = "共有元と共有履歴を照合しています…";
        UpdateControls();
        try
        {
            var loaded = await Task.Run(() => SyncCore.MissingSourceFiles(config, state, source));
            missingList.ItemsSource = loaded.Status == 0 ? loaded.Files : null;
            missingStatus.Text = loaded.Status == 0
                ? $"削除候補 {loaded.Files.Length} 件。この端末が最後に変更したファイルのうち、親フォルダーを開けたものを表示します。"
                : SyncCore.ErrorMessage(loaded.Status);
        }
        catch (Exception error) when (error is JsonException or DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            missingList.ItemsSource = null;
            missingStatus.Text = "削除候補を読み込めませんでした。";
        }
        finally
        {
            syncing = false;
            UpdateControls();
        }
    }

    private async Task DeleteMissingFile()
    {
        if (closing || syncing || writerServing || writerConfiguration is not string config
            || writerState is not string state || writerSource is not string source
            || missingList.SelectedItem is not SyncCore.MissingSourceFile file)
        {
            return;
        }
        if (MessageBox.Show(this,
            $"{string.Join("/", file.Path)} の削除を他のピアへ同期します。\n共有元のファイルはこの操作では削除しません。現行版とファイル不在を再確認してから記録します。",
            "削除を同期", MessageBoxButton.YesNo, MessageBoxImage.Warning) != MessageBoxResult.Yes)
        {
            return;
        }
        syncing = true;
        UpdateControls();
        try
        {
            var status = await Task.Run(() => SyncCore.DeleteMissingSourceFile(config, state, source, file));
            if (status == 0)
            {
                ClearWriterProtection();
                writerHistoryList.ItemsSource = null;
                writerConflictList.ItemsSource = null;
                writerCandidateList.ItemsSource = null;
                missingStatus.Text = "削除を同期変更として記録しました。候補を再確認してください。";
            }
            else
            {
                missingList.ItemsSource = null;
                missingStatus.Text = status == 5
                    ? "候補の版または共有元の状態が変わりました。もう一度確認してください。"
                    : SyncCore.ErrorMessage(status);
            }
        }
        catch (Exception error) when (error is DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            missingStatus.Text = "削除処理を読み込めませんでした。";
        }
        finally
        {
            syncing = false;
            UpdateControls();
        }
    }

    private async Task RelocateMissingFile()
    {
        if (closing || syncing || writerServing || writerConfiguration is not string config
            || writerState is not string state || writerSource is not string source
            || missingList.SelectedItem is not SyncCore.MissingSourceFile file)
        {
            return;
        }
        var picker = new OpenFileDialog
        {
            Title = "移動後のファイルを選択",
            InitialDirectory = source,
            CheckFileExists = true,
        };
        if (picker.ShowDialog(this) != true) return;
        var destination = picker.FileName;
        if (MessageBox.Show(this,
            $"{string.Join("/", file.Path)} の移動先を {destination} として同期します。\n元の名前が空いていることを確認し、選んだファイルの内容も同じ変更として記録します。",
            "移動を同期", MessageBoxButton.YesNo, MessageBoxImage.Question) != MessageBoxResult.Yes)
        {
            return;
        }
        syncing = true;
        UpdateControls();
        try
        {
            var status = await Task.Run(() => SyncCore.RelocateMissingSourceFile(config, state, source, file, destination));
            if (status == 0)
            {
                ClearWriterProtection();
                writerHistoryList.ItemsSource = null;
                writerConflictList.ItemsSource = null;
                writerCandidateList.ItemsSource = null;
                missingStatus.Text = "同じファイルIDで移動を記録しました。候補を再確認してください。";
            }
            else
            {
                missingList.ItemsSource = null;
                missingStatus.Text = status == 5
                    ? "移動先の内容・共有上の空き名・候補の版を確認してください。候補を再読み込みできます。"
                    : SyncCore.ErrorMessage(status);
            }
        }
        catch (Exception error) when (error is DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            missingStatus.Text = "移動処理を読み込めませんでした。";
        }
        finally
        {
            syncing = false;
            UpdateControls();
        }
    }

    private async Task LoadWriterProtection()
    {
        if (closing || syncing || writerServing || writerConfiguration is not string config
            || writerState is not string state)
        {
            return;
        }
        syncing = true;
        UpdateControls();
        try
        {
            var loaded = await Task.Run(() => SyncCore.WriterProtectedOriginals(config, state));
            if (loaded.Status == 0 && loaded.Report is not null)
            {
                writerOriginalList.ItemsSource = loaded.Report.Originals.Select(item => item.DisplayText()).ToArray();
                writerProtectionStatus.Text =
                    $"保護原本 {loaded.Report.Files} 件 ({loaded.Report.Bytes:N0} バイト)、未取得 {loaded.Report.Missing} 件";
            }
            else
            {
                writerOriginalList.ItemsSource = null;
                writerProtectionStatus.Text = SyncCore.ErrorMessage(loaded.Status);
            }
        }
        catch (Exception error) when (error is JsonException or DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            writerOriginalList.ItemsSource = null;
            writerProtectionStatus.Text = "保護原本の一覧を読み込めませんでした。";
        }
        finally
        {
            syncing = false;
            UpdateControls();
        }
    }

    private async Task CatchUpWriter()
    {
        if (closing || syncing || writerServing || writerConfiguration is not string config
            || writerState is not string state)
        {
            return;
        }
        syncing = true;
        using var operation = new CancellationTokenSource();
        catchUpCancellation = operation;
        writerStatus.Text = "他のピアから取得しています…";
        UpdateControls();
        try
        {
            var peers = Path.Combine(Path.GetDirectoryName(config)!, "direct.json");
            var fetched = await Task.Run(() => SyncCore.CatchUpWriter(config, state, peers, operation.Token));
            writerStatus.Text = fetched.Status == 0
                ? $"他のピアから変更 {fetched.EventsAdded} 件を取得しました。"
                : fetched.Status == 7 ? "取得を中断しました。次回は保存済みのデータを再利用します。"
                : SyncCore.ErrorMessage(fetched.Status);
            if (fetched.Status == 0)
            {
                ClearWriterProtection();
                writerHistoryList.ItemsSource = null;
                writerConflictList.ItemsSource = null;
                writerCandidateList.ItemsSource = null;
                historyStatus.Text = conflictStatus.Text = "";
            }
        }
        catch (Exception error) when (error is DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            writerStatus.Text = "ピアからの取得処理を読み込めませんでした。";
        }
        finally
        {
            catchUpCancellation = null;
            syncing = false;
            UpdateControls();
        }
    }

    private async Task RunSync()
    {
        if (closing || syncing || configuration is not string config || destination is not string root)
        {
            return;
        }
        syncing = true;
        using var operation = new CancellationTokenSource();
        cancellation = operation;
        UpdateControls();
        result.Text = "同期中…";
        rtcRouteStatus.Text = "";
        var state = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "P2PSync");
        using var bridgeStop = CancellationTokenSource.CreateLinkedTokenSource(operation.Token);
        try
        {
            var rtc = Path.Combine(Path.GetDirectoryName(config)!, "rtc.json");
            if (File.Exists(rtc))
            {
                var ready = new TaskCompletionSource(TaskCreationOptions.RunContinuationsAsynchronously);
                _ = Task.Run(() =>
                {
                    var status = SyncCore.RunRtcBridge(rtc, bridgeStop.Token, phase =>
                    {
                        if (phase == "ready") ready.TrySetResult();
                        if (phase == "failed") ready.TrySetException(new IOException("WebRTC 接続に失敗しました。"));
                        if (phase.StartsWith("route:") || phase == "relay-limit")
                            Dispatcher.BeginInvoke(new Action(() => rtcRouteStatus.Text = SyncCore.RtcStatusText(phase)));
                    });
                    ready.TrySetException(new IOException($"WebRTC 接続が終了しました: {status}"));
                });
                var completed = await Task.WhenAny(ready.Task, Task.Delay(TimeSpan.FromSeconds(20), operation.Token));
                if (completed != ready.Task) throw new IOException("WebRTC 接続がタイムアウトしました。");
                await ready.Task;
            }
            var pull = await Task.Run(() => SyncCore.Pull(config, state, root, operation.Token));
            bridgeStop.Cancel();
            result.Text = pull.Status == 0
                ? $"イベント {pull.Summary.EventsAdded} 件、新規 {pull.Summary.FilesPublished} 件、更新 {pull.Summary.FilesUpdated} 件、削除 {pull.Summary.FilesRemoved} 件、変更なし {pull.Summary.FilesPresent} 件、保留 {pull.Summary.FilesBlocked} 件\n保護原本 {pull.Summary.ProtectedFiles} 件 ({pull.Summary.ProtectedBytes:N0} バイト)、未取得 {pull.Summary.ProtectedMissing} 件\n履歴の保存容量 {pull.HistoryBytes:N0} バイト、未取得 {pull.HistoryMissing} 件"
                : SyncCore.ErrorMessage(pull.Status);
            if (pull.Status == 0)
            {
                originalList.ItemsSource = pull.Originals.Select(original => original.DisplayText()).ToArray();
                originals.Visibility = pull.Originals.Length == 0 ? Visibility.Collapsed : Visibility.Visible;
                conflictList.ItemsSource = pull.Conflicts.Select(conflict => string.Join("\n\n", conflict.Versions.Select(version =>
                    $"{version.Name ?? "削除"} · {version.Peer[..12]} / {version.Sequence}\n" +
                    (version.Kind == "deleted" ? "削除候補" :
                     version.Kind == "directory" ? "フォルダー" :
                     version.Bytes is ulong bytes ? $"{bytes:N0} バイト" : "本体未取得")))).ToArray();
                conflicts.Visibility = pull.Conflicts.Length == 0 ? Visibility.Collapsed : Visibility.Visible;
                historyList.ItemsSource = pull.History.Select(entry =>
                    $"{(entry.Path is null ? "パス不明" : string.Join("/", entry.Path))}\n" +
                    $"{entry.Content.Length:N0} バイト · {DateTimeOffset.FromUnixTimeSeconds(entry.ArchivedAt).ToLocalTime():g}").ToArray();
                history.Visibility = pull.History.Length == 0 ? Visibility.Collapsed : Visibility.Visible;
            }
        }
        catch (JsonException)
        {
            result.Text = "保護原本の一覧を読み込めませんでした。";
        }
        catch (IOException error)
        {
            result.Text = error.Message;
        }
        catch (Exception error) when (error is DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            result.Text = "対応する Rust ライブラリを読み込めませんでした。";
        }
        finally
        {
            bridgeStop.Cancel();
            syncing = false;
            cancellation = null;
            UpdateControls();
        }
    }

    private async Task LoadWriterHistory()
    {
        if (closing || syncing || writerServing || writerConfiguration is not string config || writerState is not string state)
        {
            return;
        }
        syncing = true;
        UpdateControls();
        try
        {
            var loaded = await Task.Run(() => SyncCore.WriterHistory(config, state));
            if (loaded.Status == 0)
            {
                writerHistoryList.ItemsSource = loaded.Entries;
                historyStatus.Text = $"書き込みピアの履歴 {loaded.Entries.Length} 件を読み込みました。";
            }
            else
            {
                writerHistoryList.ItemsSource = null;
                historyStatus.Text = SyncCore.ErrorMessage(loaded.Status);
            }
        }
        catch (Exception error) when (error is JsonException or DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            historyStatus.Text = "書き込みピアの履歴を読み込めませんでした。";
        }
        finally
        {
            syncing = false;
            UpdateControls();
        }
    }

    private async Task RestoreWriterHistory()
    {
        if (closing || syncing || writerServing || writerConfiguration is not string config || writerState is not string state
            || writerHistoryList.SelectedItem is not SyncCore.WriterHistoryEntry entry
            || entry.CurrentHeads.Length != 1)
        {
            return;
        }
        var name = entry.Path is null ? "パス不明" : string.Join("/", entry.Path);
        if (MessageBox.Show(this,
            $"{name} の履歴を新しい同期変更として復元します。\n現在の版が変わっていれば復元は拒否されます。",
            "履歴の復元", MessageBoxButton.YesNo, MessageBoxImage.Question) != MessageBoxResult.Yes)
        {
            return;
        }
        syncing = true;
        UpdateControls();
        try
        {
            var status = await Task.Run(() => SyncCore.RestoreWriterHistory(config, state, entry.Archive, entry.CurrentHeads[0]));
            historyStatus.Text = status == 0 ? "履歴を新しい同期変更として復元しました。" : SyncCore.ErrorMessage(status);
            if (status == 0) ClearWriterProtection();
            writerHistoryList.ItemsSource = null;
        }
        catch (Exception error) when (error is DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            historyStatus.Text = "復元処理を読み込めませんでした。";
        }
        finally
        {
            syncing = false;
            UpdateControls();
        }
    }

    private async Task DeleteWriterHistory(bool all)
    {
        if (closing || syncing || writerServing || writerConfiguration is not string config || writerState is not string state)
        {
            return;
        }
        var selected = all
            ? (writerHistoryList.ItemsSource as SyncCore.WriterHistoryEntry[] ?? [])
            : writerHistoryList.SelectedItem is SyncCore.WriterHistoryEntry entry ? [entry] : [];
        if (selected.Length == 0 || MessageBox.Show(this,
            $"履歴 {selected.Length} 件を削除します。復元には使えなくなります。\n未同期ピアのために保護中の原本は残ります。",
            "履歴の削除", MessageBoxButton.YesNo, MessageBoxImage.Warning) != MessageBoxResult.Yes)
        {
            return;
        }
        syncing = true;
        UpdateControls();
        try
        {
            var status = await Task.Run(() => SyncCore.DeleteWriterHistory(config, state, selected));
            if (status == 0)
            {
                ClearWriterProtection();
                writerHistoryList.ItemsSource = null;
                historyStatus.Text = $"履歴 {selected.Length} 件を削除しました。保護中の原本は保持されます。履歴を再読み込みしてください。";
            }
            else
            {
                historyStatus.Text = status == 5 ? "履歴が変更されました。再読み込みして選び直してください。" : SyncCore.ErrorMessage(status);
            }
        }
        catch (Exception error) when (error is DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            historyStatus.Text = "履歴の削除処理を読み込めませんでした。";
        }
        finally
        {
            syncing = false;
            UpdateControls();
        }
    }

    private async Task LoadWriterConflicts()
    {
        if (closing || syncing || writerServing || writerConfiguration is not string config || writerState is not string state)
        {
            return;
        }
        syncing = true;
        UpdateControls();
        try
        {
            var loaded = await Task.Run(() => SyncCore.WriterConflicts(config, state));
            if (loaded.Status == 0)
            {
                writerConflictList.ItemsSource = loaded.Entries;
                writerCandidateList.ItemsSource = null;
                conflictStatus.Text = $"書き込みピアの未解決の競合 {loaded.Entries.Length} 件を読み込みました。";
            }
            else
            {
                writerConflictList.ItemsSource = null;
                writerCandidateList.ItemsSource = null;
                conflictStatus.Text = SyncCore.ErrorMessage(loaded.Status);
            }
        }
        catch (Exception error) when (error is JsonException or DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            conflictStatus.Text = "書き込みピアの競合を読み込めませんでした。";
        }
        finally
        {
            syncing = false;
            UpdateControls();
        }
    }

    private async Task ResolveWriterConflict()
    {
        if (closing || syncing || writerServing || writerConfiguration is not string config || writerState is not string state
            || writerConflictList.SelectedItem is not SyncCore.WriterConflict conflict
            || writerCandidateList.SelectedItem is not SyncCore.WriterConflictVersion choice
            || conflict.Versions.Length < 2 || conflict.Versions.Any(version => version.Kind == "directory")
            || !conflict.Versions.Contains(choice))
        {
            return;
        }
        if (MessageBox.Show(this,
            $"{choice} を採用します。表示された {conflict.Versions.Length} 件の候補が変わっていれば解決は拒否されます。",
            "競合の解決", MessageBoxButton.YesNo, MessageBoxImage.Question) != MessageBoxResult.Yes)
        {
            return;
        }
        syncing = true;
        UpdateControls();
        try
        {
            var status = await Task.Run(() => SyncCore.ResolveWriterConflict(config, state,
                choice.Id, conflict.Versions.Select(version => version.Id).ToArray()));
            conflictStatus.Text = status == 0 ? "選択した版を採用する同期変更を記録しました。" :
                status == 5 ? "競合候補が変わったか、採用版の本体を確認できません。競合を読み直してください。" : SyncCore.ErrorMessage(status);
            writerConflictList.ItemsSource = null;
            writerCandidateList.ItemsSource = null;
            if (status == 0)
            {
                ClearWriterProtection();
                writerHistoryList.ItemsSource = null;
                historyStatus.Text = "";
            }
        }
        catch (Exception error) when (error is DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            conflictStatus.Text = "競合解決処理を読み込めませんでした。";
        }
        finally
        {
            syncing = false;
            UpdateControls();
        }
    }
}

internal static class SyncCore
{
    [StructLayout(LayoutKind.Sequential)]
    internal struct PullSummary
    {
        public ulong EventsAdded;
        public ulong FilesPublished;
        public ulong FilesPresent;
        public ulong FilesBlocked;
        public ulong FilesUpdated;
        public ulong FilesRemoved;
        public ulong ProtectedFiles;
        public ulong ProtectedBytes;
        public ulong ProtectedMissing;
    }

    internal sealed record WaitingPeer(
        [property: JsonPropertyName("peer")] string Peer,
        [property: JsonPropertyName("state")] string State);

    internal sealed record ProtectedOriginal(
        [property: JsonPropertyName("names")] string[] Names,
        [property: JsonPropertyName("bytes")] ulong? Bytes,
        [property: JsonPropertyName("conflict")] bool Conflict,
        [property: JsonPropertyName("waiting")] WaitingPeer[] Waiting)
    {
        public string DisplayText()
        {
            var names = Names.Length == 0 ? "名称不明" : string.Join("、", Names);
            var size = Bytes is ulong bytes ? $"{bytes:N0} バイト" : "本体未取得";
            var heading = $"{names} · {size}";
            if (Conflict)
            {
                heading += " · 競合未解決";
            }
            return string.Join("\n", Waiting.Select(peer =>
                $"{(peer.State == "events" ? "変更の受信待ち" : "ピアの確認待ち")}  {peer.Peer[..12]}")
                .Prepend(heading));
        }
    }

    internal sealed record WriterProtectedReport(
        [property: JsonPropertyName("files")] ulong Files,
        [property: JsonPropertyName("bytes")] ulong Bytes,
        [property: JsonPropertyName("missing")] ulong Missing,
        [property: JsonPropertyName("originals")] ProtectedOriginal[] Originals);

    internal sealed record ConflictVersion(
        [property: JsonPropertyName("peer")] string Peer,
        [property: JsonPropertyName("sequence")] ulong Sequence,
        [property: JsonPropertyName("name")] string? Name,
        [property: JsonPropertyName("kind")] string Kind,
        [property: JsonPropertyName("bytes")] ulong? Bytes);

    internal sealed record FileConflict(
        [property: JsonPropertyName("object")] string Object,
        [property: JsonPropertyName("versions")] ConflictVersion[] Versions);

    internal sealed record HistoryContent([property: JsonPropertyName("length")] ulong Length);
    internal sealed record HistoryEntry(
        [property: JsonPropertyName("path")] string[]? Path,
        [property: JsonPropertyName("content")] HistoryContent Content,
        [property: JsonPropertyName("archived_at")] long ArchivedAt);

    internal sealed record WriterHistoryEntry(
        [property: JsonPropertyName("id")] long Id,
        [property: JsonPropertyName("archive")] string Archive,
        [property: JsonPropertyName("replaced_by")] string ReplacedBy,
        [property: JsonPropertyName("current_heads")] string[] CurrentHeads,
        [property: JsonPropertyName("path")] string[]? Path,
        [property: JsonPropertyName("bytes")] ulong Bytes,
        [property: JsonPropertyName("archived_at")] long ArchivedAt)
    {
        public override string ToString() =>
            $"{(Path is null ? "パス不明" : string.Join("/", Path))} · {Bytes:N0} バイト\n" +
            $"{DateTimeOffset.FromUnixTimeSeconds(ArchivedAt).ToLocalTime():g}" +
            (CurrentHeads.Length == 1 ? "" : " · 現在の版が競合中");
    }

    internal sealed record WriterConflictVersion(
        [property: JsonPropertyName("id")] string Id,
        [property: JsonPropertyName("name")] string? Name,
        [property: JsonPropertyName("kind")] string Kind)
    {
        public override string ToString() =>
            $"{Name ?? "削除"} · {Id[..12]} · {(Kind == "deleted" ? "削除" : Kind == "directory" ? "フォルダー" : "ファイル")}";
    }

    internal sealed record WriterConflict(
        [property: JsonPropertyName("versions")] WriterConflictVersion[] Versions)
    {
        public override string ToString() =>
            string.Join(" / ", Versions.Select(version => version.Name ?? "削除")) +
            $" · {Versions.Length} 件の候補";
    }

    internal sealed record TreeSummary(
        [property: JsonPropertyName("files")] ulong Files,
        [property: JsonPropertyName("directories")] ulong Directories,
        [property: JsonPropertyName("skipped")] ulong Skipped);

    internal sealed record MissingSourceFile(
        [property: JsonPropertyName("path")] string[] Path,
        [property: JsonPropertyName("head")] string Head)
    {
        public override string ToString() => string.Join("/", Path);
    }

    private sealed record SyncDetails(
        [property: JsonPropertyName("originals")] ProtectedOriginal[] Originals,
        [property: JsonPropertyName("conflicts")] FileConflict[] Conflicts,
        [property: JsonPropertyName("history")] HistoryEntry[] History,
        [property: JsonPropertyName("history_bytes")] ulong HistoryBytes,
        [property: JsonPropertyName("history_missing")] ulong HistoryMissing);

    internal static (int Status, PullSummary Summary, ProtectedOriginal[] Originals, FileConflict[] Conflicts, HistoryEntry[] History, ulong HistoryBytes, ulong HistoryMissing) Pull(
        string config, string state, string destination, CancellationToken cancellation)
    {
        byte[] configBytes = Encoding.UTF8.GetBytes(config);
        byte[] stateBytes = Encoding.UTF8.GetBytes(state);
        byte[] destinationBytes = Encoding.UTF8.GetBytes(destination);
        byte[]? json = null;
        int status = PullNative(configBytes, (nuint)configBytes.Length,
            stateBytes, (nuint)stateBytes.Length, destinationBytes, (nuint)destinationBytes.Length,
            out PullSummary summary, (_, data, length) =>
            {
                if (length > int.MaxValue)
                {
                    return;
                }
                json = new byte[(int)length];
                Marshal.Copy(data, json, 0, json.Length);
            }, IntPtr.Zero, _ => cancellation.IsCancellationRequested ? (byte)1 : (byte)0);
        if (status != 0)
        {
            return (status, summary, [], [], [], 0, 0);
        }
        var details = json is null ? null : JsonSerializer.Deserialize<SyncDetails>(json);
        return details is null ? (6, summary, [], [], [], 0, 0) :
            (status, summary, details.Originals, details.Conflicts, details.History, details.HistoryBytes, details.HistoryMissing);
    }

    internal static (int Status, WriterHistoryEntry[] Entries) WriterHistory(string config, string state)
    {
        var configBytes = Encoding.UTF8.GetBytes(config);
        var stateBytes = Encoding.UTF8.GetBytes(state);
        byte[]? json = null;
        var status = WriterHistoryNative(configBytes, (nuint)configBytes.Length,
            stateBytes, (nuint)stateBytes.Length, (_, data, length) =>
            {
                if (length > int.MaxValue) return;
                json = new byte[(int)length];
                Marshal.Copy(data, json, 0, json.Length);
            }, IntPtr.Zero);
        return status != 0 || json is null ? (status == 0 ? 6 : status, []) :
            (0, JsonSerializer.Deserialize<WriterHistoryEntry[]>(json) ?? []);
    }

    internal static (int Status, WriterProtectedReport? Report) WriterProtectedOriginals(string config, string state)
    {
        var configBytes = Encoding.UTF8.GetBytes(config);
        var stateBytes = Encoding.UTF8.GetBytes(state);
        byte[]? json = null;
        var status = WriterProtectedOriginalsNative(configBytes, (nuint)configBytes.Length,
            stateBytes, (nuint)stateBytes.Length, (_, data, length) =>
            {
                if (length > int.MaxValue) return;
                json = new byte[(int)length];
                Marshal.Copy(data, json, 0, json.Length);
            }, IntPtr.Zero);
        return status != 0 || json is null ? (status == 0 ? 6 : status, null) :
            (0, JsonSerializer.Deserialize<WriterProtectedReport>(json));
    }

    internal static int RestoreWriterHistory(string config, string state, string archive, string reviewed)
    {
        var configBytes = Encoding.UTF8.GetBytes(config);
        var stateBytes = Encoding.UTF8.GetBytes(state);
        var archiveBytes = Encoding.UTF8.GetBytes(archive);
        var reviewedBytes = Encoding.UTF8.GetBytes(reviewed);
        return WriterRestoreNative(configBytes, (nuint)configBytes.Length,
            stateBytes, (nuint)stateBytes.Length, archiveBytes, (nuint)archiveBytes.Length,
            reviewedBytes, (nuint)reviewedBytes.Length);
    }

    internal static int DeleteWriterHistory(string config, string state, WriterHistoryEntry[] entries)
    {
        var configBytes = Encoding.UTF8.GetBytes(config);
        var stateBytes = Encoding.UTF8.GetBytes(state);
        var selected = JsonSerializer.SerializeToUtf8Bytes(entries.Select(entry => new
        {
            id = entry.Id,
            archive = entry.Archive,
            replaced_by = entry.ReplacedBy,
            archived_at = entry.ArchivedAt,
        }).ToArray());
        return WriterDeleteHistoryNative(configBytes, (nuint)configBytes.Length,
            stateBytes, (nuint)stateBytes.Length, selected, (nuint)selected.Length);
    }

    internal static (int Status, WriterConflict[] Entries) WriterConflicts(string config, string state)
    {
        var configBytes = Encoding.UTF8.GetBytes(config);
        var stateBytes = Encoding.UTF8.GetBytes(state);
        byte[]? json = null;
        var status = WriterConflictsNative(configBytes, (nuint)configBytes.Length,
            stateBytes, (nuint)stateBytes.Length, (_, data, length) =>
            {
                if (length > int.MaxValue) return;
                json = new byte[(int)length];
                Marshal.Copy(data, json, 0, json.Length);
            }, IntPtr.Zero);
        return status != 0 || json is null ? (status == 0 ? 6 : status, []) :
            (0, JsonSerializer.Deserialize<WriterConflict[]>(json) ?? []);
    }

    internal static int ResolveWriterConflict(string config, string state, string choice, string[] reviewed)
    {
        var configBytes = Encoding.UTF8.GetBytes(config);
        var stateBytes = Encoding.UTF8.GetBytes(state);
        var choiceBytes = Encoding.UTF8.GetBytes(choice);
        var reviewedBytes = JsonSerializer.SerializeToUtf8Bytes(reviewed);
        return WriterResolveNative(configBytes, (nuint)configBytes.Length,
            stateBytes, (nuint)stateBytes.Length, choiceBytes, (nuint)choiceBytes.Length,
            reviewedBytes, (nuint)reviewedBytes.Length);
    }

    internal static int ServeWriter(string config, string state, CancellationToken cancellation,
        Action<string> onReady)
    {
        var configBytes = Encoding.UTF8.GetBytes(config);
        var stateBytes = Encoding.UTF8.GetBytes(state);
        return WriterServeNative(configBytes, (nuint)configBytes.Length,
            stateBytes, (nuint)stateBytes.Length, (_, data, length) =>
            {
                if (length > 1024) return;
                var address = new byte[(int)length];
                Marshal.Copy(data, address, 0, address.Length);
                try { onReady(Encoding.UTF8.GetString(address)); }
                catch (InvalidOperationException) { /* Window closed during startup. */ }
            }, _ => cancellation.IsCancellationRequested ? (byte)1 : (byte)0, IntPtr.Zero);
    }

    internal static int RunRtcBridge(string config, CancellationToken cancellation, Action<string> onStatus)
    {
        var configBytes = Encoding.UTF8.GetBytes(config);
        return RtcBridgeNative(configBytes, (nuint)configBytes.Length,
            (_, data, length) =>
            {
                if (length > 64) return;
                var bytes = new byte[(int)length];
                Marshal.Copy(data, bytes, 0, bytes.Length);
                onStatus(Encoding.UTF8.GetString(bytes));
            }, _ => cancellation.IsCancellationRequested ? (byte)1 : (byte)0, IntPtr.Zero);
    }

    internal static string RtcStatusText(string phase)
    {
        if (phase == "relay-limit") return "TURN中継の使用量上限に達しました。";
        if (phase == "reconnecting") return "WebRTCを再接続しています…";
        if (!phase.StartsWith("route:")) return $"WebRTC {phase}";
        var values = phase.Split(' ', StringSplitOptions.RemoveEmptyEntries);
        var route = values[0] == "route:relay" ? "TURN中継" : "直接接続";
        static string Bytes(string value) => ulong.TryParse(value.Split('=')[1], out var bytes)
            ? $"{bytes:N0} バイト" : "不明";
        return values.Length >= 3 ? $"WebRTC {route} · 送信 {Bytes(values[1])} / 受信 {Bytes(values[2])}" : $"WebRTC {route}";
    }

    internal static (int Status, TreeSummary Summary) RefreshWriterTree(string config, string state, string source)
    {
        var configBytes = Encoding.UTF8.GetBytes(config);
        var stateBytes = Encoding.UTF8.GetBytes(state);
        var sourceBytes = Encoding.UTF8.GetBytes(source);
        byte[]? json = null;
        var status = WriterRefreshTreeNative(configBytes, (nuint)configBytes.Length,
            stateBytes, (nuint)stateBytes.Length, sourceBytes, (nuint)sourceBytes.Length,
            (_, data, length) =>
            {
                if (length > 4096) return;
                json = new byte[(int)length];
                Marshal.Copy(data, json, 0, json.Length);
            }, IntPtr.Zero);
        return status != 0 || json is null ? (status == 0 ? 6 : status, new TreeSummary(0, 0, 0)) :
            (0, JsonSerializer.Deserialize<TreeSummary>(json) ?? new TreeSummary(0, 0, 0));
    }

    internal static (int Status, MissingSourceFile[] Files) MissingSourceFiles(string config, string state, string source)
    {
        var configBytes = Encoding.UTF8.GetBytes(config);
        var stateBytes = Encoding.UTF8.GetBytes(state);
        var sourceBytes = Encoding.UTF8.GetBytes(source);
        byte[]? json = null;
        var status = WriterMissingFilesNative(configBytes, (nuint)configBytes.Length,
            stateBytes, (nuint)stateBytes.Length, sourceBytes, (nuint)sourceBytes.Length,
            (_, data, length) =>
            {
                if (length > 8 * 1024 * 1024) return;
                json = new byte[(int)length];
                Marshal.Copy(data, json, 0, json.Length);
            }, IntPtr.Zero);
        return status != 0 || json is null ? (status == 0 ? 6 : status, []) :
            (0, JsonSerializer.Deserialize<MissingSourceFile[]>(json) ?? []);
    }

    internal static int DeleteMissingSourceFile(string config, string state, string source, MissingSourceFile file)
    {
        var configBytes = Encoding.UTF8.GetBytes(config);
        var stateBytes = Encoding.UTF8.GetBytes(state);
        var sourceBytes = Encoding.UTF8.GetBytes(source);
        var pathBytes = Encoding.UTF8.GetBytes(string.Join("/", file.Path));
        var headBytes = Encoding.UTF8.GetBytes(file.Head);
        return WriterDeleteMissingFileNative(configBytes, (nuint)configBytes.Length,
            stateBytes, (nuint)stateBytes.Length, sourceBytes, (nuint)sourceBytes.Length,
            pathBytes, (nuint)pathBytes.Length, headBytes, (nuint)headBytes.Length);
    }

    internal static int RelocateMissingSourceFile(string config, string state, string source,
        MissingSourceFile file, string destination)
    {
        var configBytes = Encoding.UTF8.GetBytes(config);
        var stateBytes = Encoding.UTF8.GetBytes(state);
        var sourceBytes = Encoding.UTF8.GetBytes(source);
        var pathBytes = Encoding.UTF8.GetBytes(string.Join("/", file.Path));
        var headBytes = Encoding.UTF8.GetBytes(file.Head);
        var destinationBytes = Encoding.UTF8.GetBytes(destination);
        return WriterRelocateMissingFileNative(configBytes, (nuint)configBytes.Length,
            stateBytes, (nuint)stateBytes.Length, sourceBytes, (nuint)sourceBytes.Length,
            pathBytes, (nuint)pathBytes.Length, headBytes, (nuint)headBytes.Length,
            destinationBytes, (nuint)destinationBytes.Length);
    }

    internal static (int Status, ulong EventsAdded) CatchUpWriter(string config, string state,
        string peers, CancellationToken cancellation)
    {
        var configBytes = Encoding.UTF8.GetBytes(config);
        var stateBytes = Encoding.UTF8.GetBytes(state);
        var peerBytes = Encoding.UTF8.GetBytes(peers);
        var status = WriterCatchUpNative(configBytes, (nuint)configBytes.Length,
            stateBytes, (nuint)stateBytes.Length,
            peerBytes, (nuint)peerBytes.Length, out var added,
            _ => cancellation.IsCancellationRequested ? (byte)1 : (byte)0, IntPtr.Zero);
        return (status, added);
    }

    internal static string ErrorMessage(int status) => status switch
    {
        1 or 2 => "接続設定または保存先を確認してください。",
        3 => "ローカルの設定ファイルや保存領域にアクセスできませんでした。",
        4 => "TLS 証明書または秘密鍵を読み込めませんでした。",
        5 => "同期に失敗しました。接続先、証明書、同期先を確認してください。",
        7 => "同期を中断しました。次回は保存済みのデータを再利用します。",
        8 => "古い同期状態が復元された可能性があります。共有元を変更せず、状態の復旧が必要です。",
        _ => "同期中に内部エラーが発生しました。",
    };

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate void OriginalsCallback(IntPtr context, IntPtr data, nuint length);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate byte CancellationCallback(IntPtr context);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_rtc_bridge_run_v1", CallingConvention = CallingConvention.Cdecl)]
    private static extern int RtcBridgeNative(byte[] config, nuint configLength,
        OriginalsCallback status, CancellationCallback cancellation, IntPtr context);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_pull_v6", CallingConvention = CallingConvention.Cdecl)]
    private static extern int PullNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        byte[] destination, nuint destinationLength, out PullSummary summary,
        OriginalsCallback originals, IntPtr context, CancellationCallback cancellation);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_writer_history", CallingConvention = CallingConvention.Cdecl)]
    private static extern int WriterHistoryNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        OriginalsCallback callback, IntPtr context);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_writer_protected_originals", CallingConvention = CallingConvention.Cdecl)]
    private static extern int WriterProtectedOriginalsNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        OriginalsCallback callback, IntPtr context);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_writer_restore", CallingConvention = CallingConvention.Cdecl)]
    private static extern int WriterRestoreNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        byte[] archive, nuint archiveLength, byte[] reviewed, nuint reviewedLength);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_writer_delete_history", CallingConvention = CallingConvention.Cdecl)]
    private static extern int WriterDeleteHistoryNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        byte[] selection, nuint selectionLength);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_writer_conflicts", CallingConvention = CallingConvention.Cdecl)]
    private static extern int WriterConflictsNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        OriginalsCallback callback, IntPtr context);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_writer_resolve", CallingConvention = CallingConvention.Cdecl)]
    private static extern int WriterResolveNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        byte[] choice, nuint choiceLength, byte[] reviewed, nuint reviewedLength);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_writer_serve", CallingConvention = CallingConvention.Cdecl)]
    private static extern int WriterServeNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        OriginalsCallback ready, CancellationCallback cancellation, IntPtr context);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_writer_refresh_tree", CallingConvention = CallingConvention.Cdecl)]
    private static extern int WriterRefreshTreeNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        byte[] source, nuint sourceLength, OriginalsCallback callback, IntPtr context);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_writer_missing_files", CallingConvention = CallingConvention.Cdecl)]
    private static extern int WriterMissingFilesNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        byte[] source, nuint sourceLength, OriginalsCallback callback, IntPtr context);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_writer_delete_missing_file", CallingConvention = CallingConvention.Cdecl)]
    private static extern int WriterDeleteMissingFileNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        byte[] source, nuint sourceLength, byte[] sharedPath, nuint sharedPathLength,
        byte[] reviewed, nuint reviewedLength);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_writer_relocate_missing_file", CallingConvention = CallingConvention.Cdecl)]
    private static extern int WriterRelocateMissingFileNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        byte[] source, nuint sourceLength, byte[] sharedPath, nuint sharedPathLength,
        byte[] reviewed, nuint reviewedLength, byte[] destination, nuint destinationLength);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_writer_catch_up_v2", CallingConvention = CallingConvention.Cdecl)]
    private static extern int WriterCatchUpNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        byte[] peers, nuint peersLength, out ulong eventsAdded,
        CancellationCallback cancellation, IntPtr context);
}
