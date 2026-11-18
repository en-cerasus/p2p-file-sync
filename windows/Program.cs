using System;
using System.Runtime.InteropServices;
using System.Text;
using System.Windows;
using System.Windows.Controls;

internal static class Program
{
    [STAThread]
    private static void Main()
    {
        try
        {
            uint version = SyncCore.AbiVersion();
            if (version != 1)
            {
                MessageBox.Show($"未対応の Rust ABI {version}");
                return;
            }

            var input = new TextBox { Text = "abc", Margin = new Thickness(0, 8, 0, 8) };
            var result = new TextBox
            {
                IsReadOnly = true,
                TextWrapping = TextWrapping.Wrap,
                Margin = new Thickness(0, 8, 0, 0),
            };
            var check = new Button { Content = "SHA-256 を計算", HorizontalAlignment = HorizontalAlignment.Left };
            check.Click += (_, _) =>
            {
                byte[] data = Encoding.UTF8.GetBytes(input.Text);
                var digest = new byte[32];
                int status = SyncCore.Sha256(data, (nuint)data.Length, digest, (nuint)digest.Length);
                result.Text = status == 0 ? Convert.ToHexStringLower(digest) : $"計算エラー {status}";
            };

            var panel = new StackPanel { Margin = new Thickness(24) };
            panel.Children.Add(new TextBlock { Text = $"Rust ABI {version}" });
            panel.Children.Add(input);
            panel.Children.Add(check);
            panel.Children.Add(result);

            new Application().Run(new Window
            {
                Title = "P2P Sync",
                Width = 520,
                Height = 220,
                Content = panel,
            });
        }
        catch (Exception error) when (error is DllNotFoundException or EntryPointNotFoundException or BadImageFormatException)
        {
            MessageBox.Show($"Rust ライブラリを読み込めません。\n{error.Message}");
        }
    }
}

internal static class SyncCore
{
    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_abi_version", CallingConvention = CallingConvention.Cdecl)]
    internal static extern uint AbiVersion();

    [DllImport("p2p_sync_core", EntryPoint = "p2p_sync_sha256", CallingConvention = CallingConvention.Cdecl)]
    internal static extern int Sha256(byte[] input, nuint inputLength, [Out] byte[] output, nuint outputLength);
}
