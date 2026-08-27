# Navop 個人 Fork Agent 指南

本文件是本倉庫的專用開發規則。目標是在保留 Navop 現有功能、資料格式與相容性的前提下，維護一個只面向 Windows x64 與 macOS 的個人版本。

## 產品與授權邊界

- 產品名稱、執行檔名稱、設定格式與歷史相容識別維持 `Navop` / `navop`。
- `onetcli`、`OnetCli`、`ONETCLI_*` 可能是升級、資料目錄、協定或環境變數相容層，不得僅因名稱較舊就批次改名。
- 發行 target 僅限 `x86_64-pc-windows-msvc`、`aarch64-apple-darwin`、`x86_64-apple-darwin`。
- 不新增 Linux 或 Windows x86 發行產物、CI matrix 或更新 manifest target。Linux 條件編譯碼可保留作為上游相容程式碼，除非任務明確要求刪除。
- 本倉庫受 `LICENSE-APACHE` 與 `NAVOP_LICENSE` 約束。個人修改不得移除原作者版權與授權文件；建立公開下載、商店發行、第三方再分發或競爭性產品前，必須先確認授權。

## 專案結構

- `main/`：桌面應用入口、主視窗、首頁、設定、授權、更新、Public MCP 與應用生命週期。
- `crates/core`（crate 名 `one-core`）：連線、分頁、持久化、同步、設定與共用狀態。
- `crates/ui`（crate 名 `gpui-component`）：通用 GPUI 元件與主題。
- `crates/one_ui`：Navop 專用 UI 元件。
- `crates/db` / `crates/db_view`：資料庫 backend 與 view。
- `crates/terminal` / `crates/terminal_view`：終端 backend 與 view。
- `crates/ssh`、`crates/sftp`、`crates/sftp_view`：SSH/SFTP。
- `crates/remote_desktop` / `crates/remote_desktop_view` / `crates/windows_rdp_host`：RDP/VNC 與 Windows 原生 RDP。
- `crates/extension-*`：擴充 manifest、runtime、host、WASM 與 UI。
- `crates/ai_chat_view`、`crates/agent_runtime`、`crates/tool_runtime`、`crates/public_mcp`：AI、Agent、工具與 MCP。
- `resources/`、`installer/`、`script/`、`.github/workflows/`：平台資產、安裝器、維護腳本與 CI/Release。

修改前先確認 crate 的 package 名稱；資料夾名不一定等於 `cargo -p` 使用的名稱。

## 啟動與 UI 不變量

`main/src/main.rs` 與 `main/src/onetcli_app.rs` 的初始化順序具有行為意義：

1. 先處理更新命令與開發環境變數。
2. 建立帶資產的 GPUI `Application`。
3. 先初始化 `gpui_component`，再初始化 core、one_ui 與各功能模組。
4. 設定全域狀態、資料庫狀態、registry 與通知器。
5. 每個視窗最外層必須是 `Root`，否則 dialog、sheet、notification 與鍵盤導覽可能失效。

除非有對應啟動測試與實機驗證，不得任意重排上述順序。

GPUI 修改必須同時考慮：

- flex shrink、`min_w_0`、`min_h_0` 與明確高度邊界。
- 滾動區用外層承擔 flex/裁剪，內層承擔 `overflow_*_scrollbar()`。
- Active tab 的直接 wrapper 必須截斷大型圖片、RDP frame、canvas 等 intrinsic size。
- Windows 自繪標題列的原生按鈕需要阻斷後方 drag hitbox。
- 「元素存在」不等於「可見」；UI 測試要斷言 bounds 寬高大於零並位於 viewport 內。
- 改動後要檢查正常視窗與短視窗、亮色與深色、鍵盤焦點及錯誤/載入/空狀態。

## 非同步與 runtime 規則

GPUI executor 不是 Tokio runtime，兩者不可互換：

- 純 HTTP/檔案 I/O 且不依賴 Tokio reactor 的背景工作，優先使用 `cx.background_spawn`，完成後回前景更新 entity。
- 資料庫 driver、Tokio socket、timer、process 或 channel 必須由應用持有的 Tokio runtime 執行；優先使用現有 `one_core::gpui_tokio::Tokio::*` 封裝。
- 不得從 GPUI background executor 直接 poll 依賴 Tokio reactor 的 future。
- `cx.spawn` / `AsyncApp` 中若建立 `tokio::time::timeout` 或 `sleep`，先確保已進入正確 Tokio handle，或把 timer 整體移入 Tokio task。
- UI 測試不得依賴真實 Tokio worker 的完成時序；把完成後的狀態轉移提取成可決定性測試的 contract。
- 非同步回呼更新 entity 前要考慮 view 已關閉、session 已替換、舊結果晚到與重複提交。

## 資料與安全規則

- 連線字串、密碼、Token、私鑰、資料庫內容與使用者路徑不得寫入 log、測試快照、commit 或錯誤回報。
- 破壞性 SQL、遠端檔案覆寫、刪除、更新安裝與同步 mutation 必須沿用現有確認/權限流程。
- 修正資料庫、同步、更新或 credential 行為時，優先保留舊資料與 migration 相容性。
- 更新或重連失敗不可盲目重試寫操作；逾時可能代表結果未知。
- RDP 自動重連要保留最後已呈現 frame，瞬態狀態不得讓 active tab 版面跳動。
- 擴充 reload/install/uninstall 必須依 `ExtensionKind` 刷新；語言 WASM 保持 manifest 註冊與惰性載入，不在 UI 前景同步編譯全部 parser。

## 實作流程

### 只讀、診斷與 Review

- 只讀任務先收集程式碼、log、呼叫鏈與既有測試證據，再下結論。
- Debug 不做猜測式修補；先穩定重現或建立可驗證假設，再改最小範圍。
- Review 優先報告正確性、資料損失、死鎖/競態、跨平台回歸、錯誤路徑與測試缺口。

### 行為變更

- 新功能、公共 contract、持久化、並發、狀態機或跨 crate 共用邏輯採 TDD：Red → Green → Refactor → Verify。
- 局部 bug 可先定位根因再補最貼近故障表面的回歸測試。
- 文案、圖示、樣式與明確低風險設定可直接修改，但仍需定向驗證。
- 避免順手重構無關程式；工作區可能已有使用者變更，禁止覆蓋或回退。

### 完成門禁

在聲稱完成、已修復或可合併前：

1. 檢查 `git diff --check` 與 `git status --short`。
2. 執行與改動直接相關的測試並閱讀實際輸出。
3. Rust 改動至少執行目標 crate 測試或 `cargo check -p <crate>`。
4. 共享邏輯或高風險改動補跑受影響 crate；必要時跑 `cargo test --all`。
5. 執行 `cargo fmt --check`；可能觸發 warning 的改動執行 `cargo clippy -p <crate> -- --deny warnings`。
6. UI 改動做結構測試與必要的 Windows/macOS 手工視覺驗證。
7. 無法執行的關鍵驗證要明確列出原因，不得以推測代替通過結果。

## 常用命令

```powershell
# Windows 初始化與執行
.\script\install-window.ps1
cargo run -p main

# 定向驗證
cargo check -p main
cargo test -p main
cargo test -p one-core
cargo test -p db
cargo test -p gpui-component
cargo test -p gpui-component --doc

# 品質檢查
cargo fmt --check
cargo clippy -p main -- --deny warnings
node --test script/test-supported-platforms.mjs

# 重新由高解析 PNG 產生 Windows/macOS 圖示
.\script\generate-app-icons.ps1 -SourcePng <absolute-png-path>
```

macOS 使用 `script/bootstrap` 安裝依賴；`.app`、DMG 與 ICNS 流程分別由 `script/bundle-macos.sh`、`script/bundle-macos-dmg.sh` 與 `script/generate-macos-icon.sh` 維護。

## 發行規則

- `main/Cargo.toml` 版本必須與 `v*` tag 一致。
- `CHANGELOG.md` 是 release notes 的唯一來源，維持中英文條目。
- `.github/workflows/release.yml` 的 `all` matrix 必須恰好包含兩個 macOS target 與 Windows x64。
- `.github/workflows/upload-r2.yml` 的 updater target 必須與 release matrix 同步。
- Windows 安裝器使用 `installer/windows/` 與 `resources/windows/navop.ico`。
- macOS bundle 使用 `resources/macos/Info.plist` 與 `resources/macos/Navop.icns`。
- Logo 母版是 `resources/navop-icon.png`；替換時必須同步 PNG、ICO、ICNS，並確認透明角、ICO 多尺寸 frame 與 ICNS chunk 完整。
- 發行工作流中的 Ubuntu runner 只可作為 orchestration/publish host，不代表支援 Linux 桌面產物。

## 程式風格

- 遵循 Rust 2024 與現有 `rustfmt.toml`。
- 命名、錯誤型別、事件與 action 優先沿用所在 crate 的既有模式。
- 不新增無必要的全域狀態；Entity 更新使用正確的 `App`、`Window` 或 `AsyncApp` context。
- 公共 API 變更要同步所有呼叫方、序列化格式、文件與測試。
- 註解說明「為什麼有此約束」，不要重述程式碼。
- 使用者可見文字需同步英文、簡體中文與繁體中文 locale（若該功能已有多語系）。
