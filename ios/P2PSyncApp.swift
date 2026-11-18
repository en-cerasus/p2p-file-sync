import Foundation
import SwiftUI

@main
struct P2PSyncApp: App {
    var body: some Scene {
        WindowGroup { CoreCheckView() }
    }
}

struct CoreCheckView: View {
    @State private var input = "abc"
    @State private var result = ""

    var body: some View {
        Form {
            Section("Rust ABI \(SyncCore.abiVersion)") {
                TextField("ハッシュする文字列", text: $input)
                Button("SHA-256 を計算") {
                    do {
                        let digest = try SyncCore.sha256(Data(input.utf8))
                        result = digest.map { String(format: "%02x", $0) }.joined()
                    } catch {
                        result = "計算に失敗しました。\n\(error)"
                    }
                }
                if !result.isEmpty {
                    Text(result).font(.system(.body, design: .monospaced))
                        .textSelection(.enabled)
                }
            }
        }
    }
}
