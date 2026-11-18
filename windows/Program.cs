using System;
using System.IO;
using System.Runtime.InteropServices;
using System.Text;
using System.Threading.Tasks;
using System.Windows;
using System.Windows.Controls;
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
    private string? configuration;
    private string? destination;

    public SyncWindow()
    {
        Title = "P2P Sync";
        Width = 560;
        MinWidth = 420;
        SizeToContent = SizeToContent.Height;
        var panel = new StackPanel { Margin = new Thickness(24) };
        panel.Children.Add(new TextBlock { Text = "直接同期", FontSize = 24, FontWeight = FontWeights.SemiBold });
        AddPicker(panel, configButton, configLabel, "接続設定が未選択です");
        AddPicker(panel, destinationButton, destinationLabel, "保存先が未選択です");
        syncButton.Margin = new Thickness(0, 16, 0, 0);
        syncButton.HorizontalAlignment = HorizontalAlignment.Left;
        panel.Children.Add(syncButton);
        panel.Children.Add(result);
        Content = panel;

        configButton.Click += (_, _) =>
        {
            var picker = new OpenFileDialog { Filter = "接続設定 (*.json)|*.json", CheckFileExists = true };
            if (picker.ShowDialog(this) == true)
            {
                configuration = picker.FileName;
                configLabel.Text = configuration;
                syncButton.IsEnabled = destination is not null;
            }
        };
        destinationButton.Click += (_, _) =>
        {
            var picker = new OpenFolderDialog { Title = "同期先フォルダー" };
            if (picker.ShowDialog(this) == true)
            {
                destination = picker.FolderName;
                destinationLabel.Text = destination;
                syncButton.IsEnabled = configuration is not null;
            }
        };
        syncButton.Click += async (_, _) => await RunSync();
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
        if (configuration is not string config || destination is not string root)
        {
            return;
        }
        configButton.IsEnabled = destinationButton.IsEnabled = syncButton.IsEnabled = false;
        result.Text = "同期中…";
        var state = Path.Combine(Environment.GetFolderPath(Environment.SpecialFolder.LocalApplicationData), "P2PSync");
        try
        {
            var pull = await Task.Run(() => SyncCore.Pull(config, state, root));
            result.Text = pull.Status == 0
                ? $"イベント {pull.Summary.EventsAdded} 件、新規 {pull.Summary.FilesPublished} 件、既存 {pull.Summary.FilesPresent} 件、保留 {pull.Summary.FilesBlocked} 件"
                : SyncCore.ErrorMessage(pull.Status);
        }
        catch (Exception error) when (error is DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            result.Text = "対応する Rust ライブラリを読み込めませんでした。";
        }
        finally
        {
            configButton.IsEnabled = destinationButton.IsEnabled = syncButton.IsEnabled = true;
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
    }

    internal static (int Status, PullSummary Summary) Pull(string config, string state, string destination)
    {
        byte[] configBytes = Encoding.UTF8.GetBytes(config);
        byte[] stateBytes = Encoding.UTF8.GetBytes(state);
        byte[] destinationBytes = Encoding.UTF8.GetBytes(destination);
        int status = PullNative(configBytes, (nuint)configBytes.Length,
            stateBytes, (nuint)stateBytes.Length, destinationBytes, (nuint)destinationBytes.Length,
            out PullSummary summary);
        return (status, summary);
    }

    internal static string ErrorMessage(int status) => status switch
    {
        1 or 2 => "接続設定または保存先を確認してください。",
        3 => "ローカルの設定ファイルや保存領域にアクセスできませんでした。",
        4 => "TLS 証明書または秘密鍵を読み込めませんでした。",
        5 => "同期に失敗しました。接続先、証明書、同期先を確認してください。",
        _ => "同期中に内部エラーが発生しました。",
    };

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_pull", CallingConvention = CallingConvention.Cdecl)]
    private static extern int PullNative(
        byte[] config, nuint configLength, byte[] state, nuint stateLength,
        byte[] destination, nuint destinationLength, out PullSummary summary);
}
