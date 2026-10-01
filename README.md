# AI 用量面板

桌面小程式(Rust + Tauri v2)。把這台電腦上**所有** Claude、Codex、Gemini 帳號的
「5 小時」與「每週」用量放在同一個面板,**每 5 分鐘自動更新**。

目前會抓到的帳號(以這台機器為例):Claude ×3、Codex ×2、Gemini ×1(含 Orca 管理的帳號)。

## 怎麼用

```powershell
cargo run -p ai-usage-panel                      # 開發模式
cargo build --release -p ai-usage-panel          # 產生單一執行檔
.\target\release\ai-usage-panel.exe
```

右上角「設定」:更新間隔(5 / 10 / 15 / 30 分鐘)、是否自動更新過期的登入 token。
需要重新登入的帳號,卡片上會直接給你一個可複製的指令。

## 資料從哪來

| 服務 | 帳號來源 | 怎麼查 |
|---|---|---|
| Claude | Orca 管理的每個帳號(`%APPDATA%\orca\claude-accounts\*`)與 `~\.claude` | `GET api.anthropic.com/api/oauth/usage` |
| Codex | Orca 管理的每個家目錄、`~\.codex`、Orca 執行環境 | `GET chatgpt.com/backend-api/wham/usage` |
| Gemini | Antigravity CLI(`agy`)的登入 | 執行 `agy --print "/quota" --output-format json`,**不讀任何憑證** |

這些端點和 Orca、Claude Code 的 `/usage` 用的是同一批(官方沒有公開文件)。

## 為什麼這樣設計(安全規則)

1. **官方會限流。** 自動更新最短 5 分鐘;手動更新要隔 1 分鐘;失敗後退讓 5 / 10 / 15 分鐘;
   重開程式不會馬上重查(上次結果存在 `%APPDATA%\ai-usage-panel\cache.json`)。
2. **同一個登入有多份拷貝時,只讀、不換 token。** 切帳號時,同一個登入會同時存在
   `~\.claude` 與 Orca 的資料夾(Codex 還有 Orca 執行環境)。refresh token 用一次就換新,
   換了其中一份,其他份就失效,正在跑的 CLI 會被登出。所以這類帳號只讀「最新的那份」。
3. **只有「沒有其他拷貝」的帳號、而且 token 快過期(5 分鐘內)才會換。**
   換完會重新讀檔,避免蓋掉別人剛寫的結果。新 token 的保護有三層:
   先備份到 `%APPDATA%\ai-usage-panel\recovery\`,再用「暫存檔 + 改名」寫回憑證檔,成功才刪備份;
   如果中途當機或檔案被鎖住,下次啟動會自動從備份還原。設定裡可關閉(關閉後完全唯讀)。
4. **不用 `claude setup-token`。** 它只有 `user:inference` 權限,查用量需要 `user:profile`,會被 403 擋掉
   ([claude-code#22450](https://github.com/anthropics/claude-code/issues/22450))。
5. **用誠實的 User-Agent。** Claude 的 token 端點會對 `claude-code/*` 這個字串限流(HTTP 429),
   所以第三方工具照抄就會一直換不到 token(Orca 的 `net.fetch` 也是同類問題:
   [stablyai/orca#18716](https://github.com/stablyai/orca/issues/18716))。
   本程式送 `ai-usage-panel/<版本>`,實測可以正常換(測試 #19 固定了這件事)。
6. 畫面與快取檔都**不含任何 token**。

## 開發

```powershell
cargo test -p usage-core                                   # 43 項測試
cargo clippy --workspace --all-targets -- -D warnings      # 嚴格檢查
cargo run -p usage-core --bin probe -- --accounts          # 不連網:列出帳號、哪些是共用登入
cargo run -p usage-core --bin probe -- --no-refresh        # 真實查詢,不換任何 token
cargo run -p usage-core --bin probe -- --json              # 完整 JSON(不含 token)
pwsh scripts\e2e.ps1                                       # 端對端檢查(唯讀);加 -Cadence 驗證 5 分鐘自動更新
```

畫面預覽(不用開桌面程式):

```powershell
python -m http.server 8765 --directory ui
# http://127.0.0.1:8765/index.html?mock=live     全部正常
# http://127.0.0.1:8765/index.html?mock=states   每種狀態各一張卡
```

結構:`crates/usage-core`(抓資料、換 token、測試)、`src-tauri`(桌面外殼與排程)、`ui`(畫面)、
`design-system`(ui-ux-pro-max 產生的設計規則)。

## 已知限制

- 依賴 Orca 的資料夾格式(`%APPDATA%\orca\…`),Orca 改格式時要跟著改。
- 用量端點是官方內部 API,將來可能改變。
- 共用登入的 token 過期後,要用一下那個帳號(開 `claude` / `codex`)才會更新。
- 只支援 Windows。
