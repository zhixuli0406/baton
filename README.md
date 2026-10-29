# baton

讓 Claude Code 在工作中途可以依任務性質把工作交給 Codex 或 Grok 的指定模型（例如資安審查交給 Codex 的 Daybreak Blue），也能在 Claude 額度用完時，自動請另一家的 agent 接手做完。

baton 是一支 Rust 單檔 CLI，接在原生 `claude` 旁邊。它做三件事：

1. **調度**：一個 MCP server，提供 `delegate`、`quota_status`、`ask_codex`、`ask_grok`、`list_models`，以及查看與操作接手的 `handoff_list`／`handoff_show`／`handoff_stop`／`handoff_start`。實作類工作交給 `delegate`：baton 先探測各家剩餘額度，依 Codex → Grok 的順序挑還有額度的那家，再依任務類型與難度決定模型和推理深度；兩家都沒額度時回報 Claude 自己做（見[自動輪替與額度探針](#自動輪替與額度探針)）。要指定廠商或模型時用 `ask_codex`／`ask_grok`，工具說明附有各家模型的能力標籤，可傳 `capability`（如 `security`、`frontier`、`fast`、`web_search`）或直接指定 `model`。
2. **接手**：Claude 撞到訂閱額度上限時，hook 把對話整理成一份交接單，背景啟動 Codex（或你指定的 agent）接著做。Claude 回來後會自動收到接手結果。接手期間 Claude 想改檔會被擋下，兩邊不會同時改同一個 repo。
3. **多帳號**：每個 Claude 帳號一個 `CLAUDE_CONFIG_DIR`（官方的多帳號做法），共用設定、rules、skills 和對話紀錄。`baton switch <帳號>` 一個指令換帳號並接續目前的對話。**換 Claude 帳號一律手動**，baton 不會在 Claude 帳號之間自動輪替；上面說的自動輪替只發生在 Codex 與 Grok 這兩家之間。

## 前置需求

- macOS 或 Linux，Rust 1.82+，`git`
- Claude Code 2.1.234 以上（需要 `StopFailure` hook 與額度自動續跑）
- 要用的 agent CLI 已登入：
  - `codex`（Codex CLI，ChatGPT 登入）
  - `grok`（xAI Grok Build，grok.com 登入）

不支援 Google Antigravity（`agy`）：Google 官方 FAQ 明寫在 consumer 方案下「把 agy 包進第三方 coding agent」不受支援，條款第 6 條視為違約，停權會連帶封鎖 Gemini CLI 與 Code Assist。要用 Gemini 請走 API key，baton 目前沒有這條路徑。

沒裝的 CLI 不影響其他功能，呼叫時才會回報「找不到」。

## 安裝

```bash
cargo install --path .
baton install            # 只印出會做的設定變更，不動任何檔案
baton install --apply    # 寫入設定並註冊 MCP server
```

`--apply` 做三件事：

- 在 `~/.claude/settings.json` 加上三個 hook（`StopFailure`、`PreToolUse`、`UserPromptSubmit`）。原檔先備份成 `settings.json.baton-bak-<時間>`。已存在的 baton hook 不會重複加；baton 換了安裝位置時，會就地改成新路徑。
- 重新註冊 MCP server：`claude mcp remove --scope user baton`，再 `claude mcp add --scope user baton -- <baton 路徑> mcp`。
- 把 default 帳號的 user scope MCP server 合併進每個 baton 帳號（見[多個 Claude 帳號](#多個-claude-帳號)）。

## 使用

### 自動輪替與額度探針

分工是這樣：Claude 負責跟你溝通、規劃和派工，Codex 是主要的實作者，Grok 是備援，兩家都沒額度時才由 Claude 自己寫。Claude 呼叫 `delegate` 時，baton 照這個順序走：

1. 先查 Codex 額度。baton 啟動 `codex app-server` 呼叫 `account/rateLimits/read`，這個查詢不耗額度，約 1.6 秒。結果快取 5 分鐘（`probe_ttl_secs`）；同一個 Claude Code session 裡同時有好幾個派工時，只有第一個會去探測，其餘直接用它的結果。
2. Codex 額度用完就跳過，改叫 Grok。Grok CLI 沒有額度查詢 API，所以 baton 只能在 Grok 呼叫失敗、錯誤訊息看起來是額度問題時（例如 `402 Payment Required: Grok Build usage balance exhausted`），把它標為用完，封鎖 1 小時（`cooldown_secs`）。
3. 探針說還有額度，但實際呼叫時撞到額度上限（ChatGPT 端的額度狀態偶爾會跟實際不一致），baton 會記錄下來，換下一家重跑同一個任務。這種情況只封鎖到下一次該探測的時候（`probe_ttl_secs`），之後再以探針為準。探針本身失敗時，尚未到期的封鎖會保留。
4. 兩家都不能用時，回覆以 `DELEGATES_EXHAUSTED` 開頭，列出各家狀態與重置時間，Claude 收到後自己做。

額度判斷只看 CLI 結束時 stderr 的最後 5 行，比對「usage limit」「402 Payment Required」「429」這類具體片語，不看 agent 的回覆內文。`codex exec` 會把整段執行過程（包含讀到的檔案內容）寫到 stderr，所以只看結尾；這樣 agent 讀到或審查限流相關的程式碼時，不會被誤判成撞額度。

`write` 模式有一條額外規則：agent 已經改了檔案才撞額度時，**不會**換下一家接著改，以免兩家的半成品疊在一起。baton 在每次呼叫前後比對 `git status`、`git diff` 和未追蹤檔案的大小與修改時間，有差異就停下，請 Claude 先檢查工作樹。被 `.gitignore` 排除的檔案不列入比對（否則 build 產物也會算成改動），所以 agent 只改了被忽略的檔案時仍會換人。接手（handoff）的背景 worker 也照同一套輪替與停止規則。

`delegate` 依 `kind` 與 `difficulty` 挑模型和推理深度，簡單的工作不會用上最貴的模型：

| `kind` ＼ `difficulty` | `easy` | `normal`（預設） | `hard` |
|---|---|---|---|
| `code`（預設：實作、修 bug、重構、審查） | `fast`＋low<br>gpt-6-luna／grok-4.7-build-fast | `workhorse`＋medium<br>gpt-6.1-sol／grok-4.7 | `frontier`＋high<br>gpt-6-astra／grok-4.7 |
| `security`（防禦性資安審查） | `security`＋medium | `security`＋high | `security`＋xhigh |
| `research`（需要查網路上的最新文件） | `web_search`＋low | `web_search`＋medium | `web_search`＋high |

- 某家沒有對應標籤的模型時改用替代標籤：`code` 用 `workhorse`、`security` 用 `frontier`、`research` 用 `workhorse`。有原標籤的廠商排在前面，所以 `security` 先找 Codex 的 Daybreak Blue，Grok 用 grok-4.7 墊後；`research` 則先找 Grok，因為只有它能上網。
- `effort` 參數可以覆蓋表中的推理深度。
- 其餘參數（`task`、`mode`、`cwd`、`timeout_secs`）和 `ask_*` 相同，`write` 模式的範圍限制也相同。
- 回覆開頭會標出實際用了誰、跳過了誰，例如 `[delegate → grok · grok-4.7@medium · write · 312.4s · log: …]`，下一行是 `skipped: codex: exhausted until 2026-11-04 14:38 +08:00 (probe, 0m ago; weekly window 100% used; plan promax)`。

查額度：

```bash
baton quota             # 用快取（超過 5 分鐘會重新探測 Codex）
baton quota --refresh   # 強制重新探測
```

```text
codex: exhausted until 2026-11-04 14:38 +08:00 (probe, 0m ago; weekly window 100% used; plan promax)
grok: unknown (no quota API; marked exhausted only after a failed call)
rotation: enabled, order codex → grok
```

MCP 工具 `quota_status`（參數 `refresh`，預設 false）印出同樣內容。

`ask_codex`／`ask_grok` 不輪替，但也會用到額度紀錄：快取裡記錄該家已用完時直接回錯誤（不會為此去探測），呼叫中撞到額度上限時也會記錄。`ask_*` 和 `delegate` 用同一套快取：超過 `probe_ttl_secs` 就重新探測 Codex，所以加購額度後最多 5 分鐘就會生效。要立刻生效，用 `baton quota --refresh` 或 `quota_status` 搭配 `refresh=true`；對 Grok 來說，這會清掉它的額度錯誤紀錄，下一次呼叫若還是失敗會再記一次。

### 指定廠商：ask_codex 與 ask_grok

裝好後直接對 Claude 說就行，例如「請 Codex 幫你 review 這個 diff」「資安的部分找 Daybreak 看一次」「讓 Grok 查一下最新版文件」。`ask_codex` 與 `ask_grok` 共用以下參數：

| 參數 | 說明 |
|---|---|
| `task` | 交代給對方的完整任務，上限 100,000 字元。對方看不到 Claude 的對話，必須寫成自給自足的說明 |
| `mode` | `read_only`（預設，只能讀）或 `write`（可以改工作目錄裡的檔案） |
| `cwd` | 工作目錄，預設是 Claude Code 啟動的目錄 |
| `model` | 指定模型 ID（見下方模型表）。有給就以它為準 |
| `capability` | 任務需要的能力標籤，baton 挑該 agent 第一個帶此標籤的模型；`model` 有給時忽略 |
| `effort` | 推理深度 `low`／`medium`／`high`／`xhigh`／`max`；省略用模型預設。codex 以 `-c model_reasoning_effort` 傳遞，grok 以 `--reasoning-effort` |
| `timeout_secs` | 預設 900，上限 3600 |

`list_models` 不帶參數，印出完整模型表（不會啟動任何 agent）。

#### 依能力選模型

能力標籤與內建模型表（取自各家 CLI 自己的模型目錄，2026-10-27）：

| 標籤 | 意思 | codex | grok |
|---|---|---|---|
| `security` | 防禦性資安：漏洞審查、威脅建模、加固、安全程式碼審查 | `gpt-daybreak-blue-latest`（Daybreak Blue） | 無 |
| `frontier` | 最難的設計與除錯問題 | `gpt-6-astra` | `grok-4.7` |
| `workhorse` | 日常開發與審查 | `gpt-6.1-sol`（500k context） | `grok-4.7` |
| `fast` | 簡單、機械性的工作，快且便宜 | `gpt-6-luna` | `grok-4.7-build-fast` |
| `long_context` | 一次讀大量程式碼 | `gpt-6.1-sol` | 無 |
| `web_search` | 需要查最新文件、版本、新聞 | 無 | `grok-4.7` |
| `legacy` | 舊世代，只拿來對照 | `gpt-6-sol`、`gpt-5.6-*` | `grok-4.6`、`grok-4.5` |

同一個標籤有多個模型時取表中排在前面的。某個 agent 沒有那個標籤（例如對 grok 要 `security`）會回一則工具錯誤，列出它有的標籤，Claude 會改叫另一家。回覆開頭會標出實際用的模型與 effort，例如 `[codex · gpt-daybreak-blue-latest@high · read_only · 15.4s · log: …]`。

新模型不用等 baton 改版：在 `~/.baton/config.toml` 加 `[[catalog]]` 就能新增或覆蓋一列（見下方設定檔）。

`write` 模式有三個限制，因為工具參數由模型決定，而模型可能被 repo 內容或網頁誘導：

- `cwd` 必須位於 git repo 內，且在 Claude Code 啟動的 repo 之內，或目標 repo 的根目錄位於設定檔的 `[write] allowed_roots` 之下。後者不要求 Claude Code 在 repo 裡啟動；預設沒有額外允許的根目錄。
- 家目錄、家目錄的上層目錄與 `/` 不能作為目標 repo、啟動的 repo 或允許的根目錄，不存在的根目錄會略過。範圍比對與啟動 agent 都使用解析後的實際路徑。
- 目標 repo 正在接手時拒絕。

**信任前提**：baton 信任啟動時的環境，包括 `HOME` 與用來尋找 `git` 的 `PATH`。範圍檢查在 agent 啟動前執行，原本就能替換允許範圍內目錄的程序仍可能趁檢查與啟動之間更動路徑。若 `core.worktree` 將工作目錄重定位，導致 Git 回報的根目錄與實體 `.git` 邊界不符，baton 會拒絕寫入。

每次完成的呼叫都會把任務和回覆存到 `~/.baton/runs/`。被取消的呼叫不會留紀錄。

各 CLI 的權限對應（2026-10-21 實測；只靠 grok 的 `plan` 模式擋不住改檔，所以 grok 兩種模式都用工具白名單）：

| | `read_only` | `write` |
|---|---|---|
| codex | `--sandbox read-only` | `--sandbox workspace-write`（可在沙箱內跑指令） |
| grok | `plan`＋只開讀檔、搜尋、web_search | `acceptEdits`＋讀寫檔、搜尋、web_search；**不能跑指令** |

`read_only` 只防止被叫去的 agent 改檔，並不能防止它讀取工作目錄以外的檔案。

### 額度用完時自動接手

不用另外操作。Claude 停在「You've hit your session limit」時：

1. `StopFailure` hook 讀取這段對話，產出交接單 `~/.baton/handoffs/<id>/brief.md`，內容包括原始需求、最近的對話、todo、改過的檔案和 `git status`。常見格式的金鑰與私鑰會先遮蔽，但**交接單會送給接手 agent 的廠商**（預設 OpenAI）。
2. 背景啟動接手的 agent（預設 codex、`write` 模式），工作範圍是整個 repo。該 agent 額度用完時，照 `[rotation] order` 換下一家（規則同 `delegate`，已改過檔案就不換）；換過人會寫在 `handoff list` 的備註和報告開頭。結果寫進 `result.md`，完成時跳出桌面通知。
3. 接手期間 Claude 若因自動續跑而回來，它的 Edit、Write、Bash 以及 `write` 模式的 `ask_*` 都會被擋下，並告知目前由誰在接手。
4. 接手完成後，在撞到額度的那個 session 裡，Claude 的下一個工具動作或你的下一則訊息，會帶上接手報告（只送一次，不送給 subagent）。報告標示為「其他模型寫的資料」，Claude 會先用 git diff 檢查再接著做。

接手的 agent 被要求不准 commit、push、reset，不准改 `.claude/`、`.mcp.json`、`.git/hooks` 這類 agent 與 git 設定；遇到需要人判斷的地方就停下來說明。如果它還是動了這些設定檔，報告開頭會出現警告，`handoff list` 也會標出來，因為這些檔案之後可能在沙箱外執行程式。

```bash
baton handoff list            # 列出接手紀錄
baton handoff show <id>       # 看交接單與結果
baton handoff stop <id>       # 中止進行中的接手
baton handoff start           # 手動把這個目錄最近的 Claude 對話交出去
                              #   選項：--agent codex|grok、--mode read_only|write、--cwd <路徑>、--wait（前景執行）
```

同樣四個動作也有 MCP 工具版，Claude 在對話裡就能用：`handoff_list`、`handoff_show <id>` 只讀；`handoff_stop <id>` 會終止接手並解除改檔封鎖，`handoff_start`（參數 `agent`、`mode`、`cwd` 皆選填，預設取設定檔）會在背景把目前 repo 最近的對話交出去。工具說明已寫明後兩者只在使用者要求時使用；`handoff_show` 回傳的報告一律框在 `<handoff_report>` 內標示為資料，而且不會把它記成「已送達」，撞額度的那個 session 仍會照常收到一次。

設定檔在 `~/.baton/config.toml`，內容與預設值：

```toml
[handoff]
enabled = true
agent = "codex"        # codex | grok
mode = "write"         # write | read_only
timeout_secs = 3600

[write]
allowed_roots = []     # 預設為空；可填 ["~/Project"]，家目錄、家目錄的上層目錄與 / 永遠不算
# 只接受絕對路徑、~/ 開頭或 ~；相對路徑會讓設定載入失敗

[rotation]             # delegate 與接手 worker 的自動輪替
enabled = true         # false：只用 order 的第一家（接手則只用 [handoff] agent），不換人
order = ["codex", "grok"]
probe_ttl_secs = 300   # Codex 探針結果沿用多久
cooldown_secs = 3600   # 不知道重置時間時（Grok，或探針失敗），撞額度後封鎖多久

[models]               # 選填：各 agent 的預設模型（沒給 model 也沒給 capability 時用）
# codex = "gpt-6.1-sol"

# 選填：新增或覆蓋模型表的一列。agent 與 model 相同就覆蓋內建列，否則加在最後。
# [[catalog]]
# agent = "grok"
# model = "grok-5"
# tags = ["frontier", "web_search"]
# note = "grok-5: 2026-11 推出"
```

### 多個 Claude 帳號

```bash
baton account add work           # 建立帳號目錄並開啟登入流程
baton account list               # 列出帳號、email、方案、5 小時/7 天用量（讀本機快取）
baton run work                   # 用 work 帳號開 claude（後面可接 claude 的參數：baton run work -- --model opus）
baton switch work                # 用 work 帳號開 claude --continue，接續這個目錄最近的對話
baton account sync               # 修復共用 symlink，並合併 default 的 MCP server
baton account remove work --yes  # 登出並刪除帳號目錄
```

`default` 指的是 `~/.claude` 本身。其他帳號放在 `~/.baton/accounts/<名稱>/`，以 symlink 共用 `~/.claude` 的 `CLAUDE.md`、`settings.json`、`rules`、`skills`、`agents`、`commands`、`plugins`、`projects`、`file-history` 等。因為 `projects` 是共用的，換帳號之後 `--continue` 找得到原本的對話。各帳號的登入憑證與 `.claude.json` 互相獨立。

user scope 的 MCP server 設定存在各帳號的 `.claude.json` 裡。`account add`、`account sync`、`install --apply` 會把 default 的 server 合併過去：同名的以 default 為準，帳號自己另外加的會保留。

換帳號注意事項：

- 不要用 `/logout` 換帳號，它會撤銷 refresh token。
- 換帳號後 prompt cache 會失效，接續長對話的第一則訊息會比較耗額度。
- Anthropic 的條款把訂閱方案的用量上限定位為「一般個人使用」，[新版 AUP](https://www.anthropic.com/legal/aup)（2026-11-12 生效）禁止用多個帳號規避限制。baton 刻意不在 Claude 帳號之間自動輪替，帳號要怎麼用由你自己判斷。

### 資料存放

`~/.baton` 權限 0700，裡面的檔案 0600。內容包括呼叫紀錄（`runs/`）、交接單與報告（`handoffs/`）、各家額度狀態（`quota.json`）、hook 事件紀錄（`events.jsonl`，用來比對實際撞額度時 Claude Code 送出的訊息）。

## 開發

```bash
cargo build
cargo test
cargo clippy --all-targets -- -D warnings
```

給測試用的環境變數：`BATON_HOME` 改資料目錄（預設 `~/.baton`）；`BATON_CODEX_BIN`、`BATON_GROK_BIN`、`BATON_CLAUDE_BIN` 換掉實際執行的 binary。

## License

雙授權，任選其一：

- [Apache License 2.0](LICENSE-APACHE)
- [MIT License](LICENSE-MIT)

除非你另外明確聲明，你提交給本專案的貢獻都依上述雙授權釋出，不附加其他條款。
