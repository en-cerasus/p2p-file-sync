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
    private readonly Button loadWriterHistoryButton = new() { Content = "復元できる履歴を確認", Margin = new Thickness(0, 12, 0, 0) };
    private readonly ListBox writerHistoryList = new() { MaxHeight = 240 };
    private readonly Button restoreHistoryButton = new() { Content = "選択した履歴を復元", Margin = new Thickness(0, 8, 0, 0) };
    private readonly Button loadWriterConflictsButton = new() { Content = "競合を確認", Margin = new Thickness(0, 16, 0, 0) };
    private readonly ListBox writerConflictList = new() { MaxHeight = 180 };
    private readonly ListBox writerCandidateList = new() { MaxHeight = 180 };
    private readonly Button resolveConflictButton = new() { Content = "選択した版を採用", Margin = new Thickness(0, 8, 0, 0) };
    private readonly CheckBox autoSync = new()
    {
        Content = "10 秒ごとに自動同期",
        Margin = new Thickness(0, 12, 0, 0),
        IsEnabled = false,
    };
    private readonly DispatcherTimer syncTimer = new() { Interval = TimeSpan.FromSeconds(10) };
    private readonly System.Windows.Forms.NotifyIcon tray = new();
    private readonly System.Windows.Forms.ToolStripMenuItem traySync = new("同期を開始");
    private bool syncing;
    private bool closing;
    private string? configuration;
    private string? destination;
    private string? writerConfiguration;
    private string? writerState;

    public SyncWindow()
    {
        Title = "P2P Sync";
        Width = 560;
        MinWidth = 420;
        Height = 760;
        MinHeight = 440;
        var panel = new StackPanel { Margin = new Thickness(24) };
        panel.Children.Add(new TextBlock { Text = "直接同期", FontSize = 24, FontWeight = FontWeights.SemiBold });
        AddPicker(panel, configButton, configLabel, "接続設定が未選択です");
        AddPicker(panel, destinationButton, destinationLabel, "保存先が未選択です");
        syncButton.Margin = new Thickness(0, 16, 0, 0);
        syncButton.HorizontalAlignment = HorizontalAlignment.Left;
        panel.Children.Add(syncButton);
        panel.Children.Add(cancelButton);
        panel.Children.Add(autoSync);
        panel.Children.Add(result);
        originals.Content = originalList;
        panel.Children.Add(originals);
        conflicts.Content = conflictList;
        panel.Children.Add(conflicts);
        history.Content = historyList;
        panel.Children.Add(history);
        panel.Children.Add(new TextBlock { Text = "履歴の復元", FontSize = 18, Margin = new Thickness(0, 20, 0, 0) });
        AddPicker(panel, writerConfigButton, writerConfigLabel, "書き込みピア設定が未選択です");
        AddPicker(panel, writerStateButton, writerStateLabel, "状態フォルダーが未選択です");
        panel.Children.Add(loadWriterHistoryButton);
        panel.Children.Add(writerHistoryList);
        panel.Children.Add(restoreHistoryButton);
        panel.Children.Add(new TextBlock { Text = "競合の解決", FontSize = 18, Margin = new Thickness(0, 20, 0, 0) });
        panel.Children.Add(loadWriterConflictsButton);
        panel.Children.Add(writerConflictList);
        panel.Children.Add(new TextBlock { Text = "採用する版", Margin = new Thickness(0, 8, 0, 0) });
        panel.Children.Add(writerCandidateList);
        panel.Children.Add(resolveConflictButton);
        Content = new ScrollViewer { Content = panel, VerticalScrollBarVisibility = ScrollBarVisibility.Auto };
        RestorePreferences();
        var trayMenu = new System.Windows.Forms.ContextMenuStrip();
        trayMenu.Items.Add("画面を開く", null, (_, _) => ShowWindow());
        trayMenu.Items.Add(traySync);
        trayMenu.Items.Add(new System.Windows.Forms.ToolStripSeparator());
        trayMenu.Items.Add("終了", null, (_, _) => Close());
        tray.ContextMenuStrip = trayMenu;
        tray.Icon = System.Drawing.SystemIcons.Application;
        tray.Text = "P2P Sync";
        tray.Visible = true;
        tray.DoubleClick += (_, _) => ShowWindow();
        traySync.Click += async (_, _) => await RunSync();
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
                SavePreferences();
            }
        });
        loadWriterHistoryButton.Click += async (_, _) => await LoadWriterHistory();
        writerHistoryList.SelectionChanged += (_, _) => UpdateControls();
        restoreHistoryButton.Click += async (_, _) => await RestoreWriterHistory();
        loadWriterConflictsButton.Click += async (_, _) => await LoadWriterConflicts();
        writerConflictList.SelectionChanged += (_, _) =>
        {
            writerCandidateList.ItemsSource = (writerConflictList.SelectedItem as SyncCore.WriterConflict)?.Versions;
            UpdateControls();
        };
        writerCandidateList.SelectionChanged += (_, _) => UpdateControls();
        resolveConflictButton.Click += async (_, _) => await ResolveWriterConflict();
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
        string? WriterConfiguration, string? WriterState);

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
            configLabel.Text = configuration ?? "接続設定をもう一度選択してください";
            destinationLabel.Text = destination ?? "保存先をもう一度選択してください";
            writerConfigLabel.Text = writerConfiguration ?? "書き込みピア設定を選択してください";
            writerStateLabel.Text = writerState ?? "書き込みピアの状態フォルダーを選択してください";
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
                writerConfiguration, writerState);
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
        writerConfigButton.IsEnabled = writerStateButton.IsEnabled = !syncing;
        loadWriterHistoryButton.IsEnabled = !syncing && writerConfiguration is not null && writerState is not null;
        loadWriterConflictsButton.IsEnabled = loadWriterHistoryButton.IsEnabled;
        restoreHistoryButton.IsEnabled = loadWriterHistoryButton.IsEnabled
            && writerHistoryList.SelectedItem is SyncCore.WriterHistoryEntry { CurrentHeads.Length: 1 };
        resolveConflictButton.IsEnabled = loadWriterConflictsButton.IsEnabled
            && writerConflictList.SelectedItem is SyncCore.WriterConflict { Versions.Length: >= 2 } conflict
            && !conflict.Versions.Any(version => version.Kind == "directory")
            && writerCandidateList.SelectedItem is SyncCore.WriterConflictVersion;
        syncButton.IsEnabled = ready && !syncing;
        traySync.Enabled = ready && !syncing;
        tray.Text = syncing ? "P2P Sync - 同期中" : "P2P Sync";
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

    private static void AddPicker(StackPanel panel, Button button, TextBlock label, string placeholder)
    {
        button.Margin = new Thickness(0, 16, 0, 4);
        button.HorizontalAlignment = HorizontalAlignment.Left;
        label.Text = placeholder;
        panel.Children.Add(button);
        panel.Children.Add(label);
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
        var state = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "P2PSync");
        try
        {
            var pull = await Task.Run(() => SyncCore.Pull(config, state, root, operation.Token));
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
        catch (Exception error) when (error is DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            result.Text = "対応する Rust ライブラリを読み込めませんでした。";
        }
        finally
        {
            syncing = false;
            cancellation = null;
            UpdateControls();
        }
    }

    private async Task LoadWriterHistory()
    {
        if (closing || syncing || writerConfiguration is not string config || writerState is not string state)
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
                result.Text = $"書き込みピアの履歴 {loaded.Entries.Length} 件を読み込みました。";
            }
            else
            {
                result.Text = SyncCore.ErrorMessage(loaded.Status);
            }
        }
        catch (Exception error) when (error is JsonException or DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            result.Text = "書き込みピアの履歴を読み込めませんでした。";
        }
        finally
        {
            syncing = false;
            UpdateControls();
        }
    }

    private async Task RestoreWriterHistory()
    {
        if (closing || syncing || writerConfiguration is not string config || writerState is not string state
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
            result.Text = status == 0 ? "履歴を新しい同期変更として復元しました。" : SyncCore.ErrorMessage(status);
            writerHistoryList.ItemsSource = null;
        }
        catch (Exception error) when (error is DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            result.Text = "復元処理を読み込めませんでした。";
        }
        finally
        {
            syncing = false;
            UpdateControls();
        }
    }

    private async Task LoadWriterConflicts()
    {
        if (closing || syncing || writerConfiguration is not string config || writerState is not string state)
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
                result.Text = $"書き込みピアの未解決の競合 {loaded.Entries.Length} 件を読み込みました。";
            }
            else
            {
                result.Text = SyncCore.ErrorMessage(loaded.Status);
            }
        }
        catch (Exception error) when (error is JsonException or DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            result.Text = "書き込みピアの競合を読み込めませんでした。";
        }
        finally
        {
            syncing = false;
            UpdateControls();
        }
    }

    private async Task ResolveWriterConflict()
    {
        if (closing || syncing || writerConfiguration is not string config || writerState is not string state
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
            result.Text = status == 0 ? "選択した版を採用する同期変更を記録しました。" :
                status == 5 ? "競合候補が変わったか、採用版の本体を確認できません。競合を読み直してください。" : SyncCore.ErrorMessage(status);
            writerConflictList.ItemsSource = null;
            writerCandidateList.ItemsSource = null;
        }
        catch (Exception error) when (error is DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            result.Text = "競合解決処理を読み込めませんでした。";
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
        [property: JsonPropertyName("archive")] string Archive,
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

    internal static string ErrorMessage(int status) => status switch
    {
        1 or 2 => "接続設定または保存先を確認してください。",
        3 => "ローカルの設定ファイルや保存領域にアクセスできませんでした。",
        4 => "TLS 証明書または秘密鍵を読み込めませんでした。",
        5 => "同期に失敗しました。接続先、証明書、同期先を確認してください。",
        7 => "同期を中断しました。次回は保存済みのデータを再利用します。",
        _ => "同期中に内部エラーが発生しました。",
    };

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate void OriginalsCallback(IntPtr context, IntPtr data, nuint length);

    [UnmanagedFunctionPointer(CallingConvention.Cdecl)]
    private delegate byte CancellationCallback(IntPtr context);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_pull_v6", CallingConvention = CallingConvention.Cdecl)]
    private static extern int PullNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        byte[] destination, nuint destinationLength, out PullSummary summary,
        OriginalsCallback originals, IntPtr context, CancellationCallback cancellation);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_writer_history", CallingConvention = CallingConvention.Cdecl)]
    private static extern int WriterHistoryNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        OriginalsCallback callback, IntPtr context);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_writer_restore", CallingConvention = CallingConvention.Cdecl)]
    private static extern int WriterRestoreNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        byte[] archive, nuint archiveLength, byte[] reviewed, nuint reviewedLength);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_writer_conflicts", CallingConvention = CallingConvention.Cdecl)]
    private static extern int WriterConflictsNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        OriginalsCallback callback, IntPtr context);

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_writer_resolve", CallingConvention = CallingConvention.Cdecl)]
    private static extern int WriterResolveNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        byte[] choice, nuint choiceLength, byte[] reviewed, nuint reviewedLength);
}
