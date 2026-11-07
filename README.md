# baton

讓 Claude Code 在工作中途可以依任務性質把工作交給 Codex 或 Grok 的指定模型（例如資安審查交給 Codex 的 Daybreak Blue），也能在 Claude 額度用完時，自動請另一家的 agent 接手做完。

baton 是一支 Rust 單檔 CLI，接在原生 `claude` 旁邊。它做三件事：

1. **調度**：一個 MCP server，提供 `delegate`、`quota_status`、`ask_codex`、`ask_grok`、`list_models`，以及查看與操作接手的 `handoff_list`／`handoff_show`／`handoff_stop`／`handoff_start`。實作類工作交給 `delegate`：baton 先探測各家剩餘額度，依 Codex → Grok 的順序挑還有額度的那家，再依任務類型與難度決定模型和推理深度；兩家都沒額度時回報 Claude 自己做（見[自動輪替與額度探針](#自動輪替與額度探針)）。要指定廠商或模型時用 `ask_codex`／`ask_grok`，工具說明附有各家模型的能力標籤，可傳 `capability`（如 `security`、`frontier`、`fast`、`web_search`）或直接指定 `model`。
2. **接手**：Claude 撞到訂閱額度上限時，hook 把對話整理成一份交接單，背景啟動 Codex（或你指定的 agent）接著做。Claude 回來後會自動收到接手結果。接手期間 Claude 想改檔會被擋下，兩邊不會同時改同一個 repo。
3. **多帳號**：每個 Claude 帳號一個 `CLAUDE_CONFIG_DIR`（官方的多帳號做法），共用設定、rules、skills 和對話紀錄。`baton switch <帳號>` 一個指令換帳號並接續目前的對話。你正在對話的那個 Claude 帳號，baton 不會自動換掉；要換一律手動。另外可以選擇指定一個帳號當實作 worker（`ask_claude`，預設關閉），把它排進 `delegate` 的輪替順序（見 [Claude worker](#claude-worker選用預設關閉)）。

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
2. Codex 額度用完時先嘗試重置券（見下方）；仍不能用才跳過，改叫 Grok。Grok CLI 沒有額度查詢 API，所以 baton 只能在 Grok 呼叫失敗、錯誤訊息看起來是額度問題時（例如 `402 Payment Required: Grok Build usage balance exhausted`），把它標為用完，封鎖 1 小時（`cooldown_secs`）。
3. 探針說還有額度，但實際呼叫時撞到額度上限（ChatGPT 端的額度狀態偶爾會跟實際不一致），baton 會記錄下來；Codex 成功恢復額度且未改檔時，重試同一步一次，否則依下方規則停止或換下一家。探針與呼叫不一致時只封鎖到下一次該探測的時候（`probe_ttl_secs`），之後再以探針為準。探針本身失敗時，尚未到期的封鎖會保留。
4. 兩家都不能用時，回覆以 `DELEGATES_EXHAUSTED` 開頭，列出各家狀態與重置時間，Claude 收到後自己做。

額度判斷只看 CLI 結束時 stderr 的最後 5 行，比對「usage limit」「402 Payment Required」「429」這類具體片語，不看 agent 的回覆內文。`codex exec` 會把整段執行過程（包含讀到的檔案內容）寫到 stderr，所以只看結尾；這樣 agent 讀到或審查限流相關的程式碼時，不會被誤判成撞額度。

Codex 用完額度時，`delegate`、`ask_codex` 與接手 worker 預設會先嘗試兌換一張可用的額度重置券，恢復後繼續用 Codex。**重置券會自動花掉，而且無法撤回**；不想自動兌換，在 `[rotation]` 設定 `auto_reset = false`。`reset_min_wait_secs` 預設 3600：距離自然重置不到一小時就保留券，照原本規則輪替或回錯誤。工作區的額度／支出限制不會用個人券處理。

多個 baton 程序共用兌換鎖與同一額度視窗的冪等鍵，每次嘗試在送出兌換前先寫一行 `pending`，結束後再寫一行最終結果，記在 `~/.baton/resets.jsonl`，兩行共用券 ID 與冪等鍵。只有 `pending` 而沒有最終結果，表示結果未知。每個工作最多兌換一張券，包含各步驟與重試。呼叫中撞額度而成功恢復時，`delegate` 與接手 worker 只在工作樹未改動時重試同一步一次；`ask_codex` 會提示再呼叫一次，不自動重跑。回覆的 `note:` 會說明已兌換，與輪替的 `skipped:` 分開。

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
baton quota             # 只查詢，用快取（超過 5 分鐘會重新探測 Codex），不花重置券
baton quota --refresh   # 強制重新探測，仍不花重置券
```

```text
codex: exhausted until 2026-11-04 14:38 +08:00 (probe, 0m ago; weekly window 100% used; plan promax; 1 reset credit available)
grok: unknown (no quota API; marked exhausted only after a failed call)
rotation: enabled, order codex → grok
```

MCP 工具 `quota_status`（參數 `refresh`，預設 false）印出同樣內容，只查詢，永遠不花重置券。

`ask_codex`／`ask_grok` 不輪替，但也會用到額度紀錄：該家已用完、且 Codex 無法用重置券恢復時直接回錯誤，呼叫中撞到額度上限時也會記錄。`ask_*` 和 `delegate` 用同一套快取：超過 `probe_ttl_secs` 就重新探測 Codex，所以加購額度後最多 5 分鐘就會生效。要立刻查到最新狀態，用 `baton quota --refresh` 或 `quota_status` 搭配 `refresh=true`；對 Grok 來說，這會清掉它的額度錯誤紀錄，下一次呼叫若還是失敗會再記一次。

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

### Claude worker（選用，預設關閉）

把另一個 Claude 訂閱帳號當成實作者，用法跟 Codex、Grok 一樣。先用 `baton account add <名稱>` 建好帳號並登入，再在設定檔寫：

```toml
[claude]
account = "work"                        # 跑 worker 的 baton 帳號名稱；留空就關閉

[rotation]
order = ["codex", "grok", "claude"]     # 要讓 delegate 用到它，得自己加進來
```

設好之後會多一個 `ask_claude` 工具，參數與 `ask_codex` 相同。`delegate` 和接手 worker 照 `order` 輪替時也會用到它；`[handoff] agent = "claude"` 可以讓它直接接手。模型表內建 `claude-opus-5-5`（`frontier`）、`claude-sonnet-5-5`（`workhorse`）、`claude-haiku-5-5`（`fast`）。

baton 用 `CLAUDE_CONFIG_DIR=<該帳號目錄> claude -p` 啟動它，限制如下：

- 只開放 `Read`、`Grep`、`Glob`；`write` 模式再加 `Edit`、`Write`，自動接受工作目錄內的修改。不能跑 shell 指令或抓網頁，原因和 Grok 的 `write` 模式一樣：指令沒有沙箱。build 和測試要由呼叫它的 Claude 自己跑。
- 不載入任何 MCP server（`--strict-mcp-config`），所以它碰不到 baton，不會再往下派工。
- hook 全部關閉（`disableAllHooks`），另外 baton 的 hook 看到 `BATON_WORKER=1` 也會直接略過，worker 撞到額度時不會觸發接手。
- 不保存對話（`--no-session-persistence`）。各帳號共用 `projects` 目錄，存下來的話，`baton switch` 的 `--continue` 可能會接到 worker 的對話。
- 附加一段 system prompt，要它自己寫程式碼。共用的 CLAUDE.md 裡如果有「交給 Codex 寫」之類的規則，以這段為準。
- 環境變數裡的 `ANTHROPIC_*` 和 `CLAUDE*` 先清掉，再設定該帳號的 `CLAUDE_CONFIG_DIR`。`ANTHROPIC_API_KEY` 不會蓋掉訂閱登入。

`account` 指向的帳號跟呼叫者是同一個時（例如兩邊都是 `default`），baton 視為不可用並跳過。額度判斷有兩種來源：呼叫前先讀該帳號本機快取的 5 小時／7 天用量，已滿就跳過；呼叫結果回報 `You've hit your limit` 這類錯誤時標為用完，能解析出重置時間就封鎖到那個時間，解析不出來就封鎖 `cooldown_secs`。快取是那個帳號上次互動時留下的，可能已經過時。

使用前先看[多個 Claude 帳號](#多個-claude-帳號)最後一段的條款說明。

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

### 用量控制：Codex 子代理、memories 與並行上限

baton 啟動的每個 `codex exec`（`ask_codex`、`delegate`、接手 worker）都會加上 `-c features.multi_agent=false -c features.memories=false`。實測 2026-10-28～11-07 的 baton 派工中，Codex 自己再開的子代理（code-reviewer、worker、security-reviewer）約占總 token 的 37%，memories 每次會多帶約 34k 字元；派工時審查由 Claude 負責，所以關掉。直接開 Codex（桌面版、`codex` TUI）不受影響。

同時執行的 CLI 數量有上限，由 `[concurrency]` 設定，預設 Codex 與 Grok 各 4 個，`0` 表示不限。上限跨所有 baton 程序共用（每個 Claude Code session 各有一個 MCP server），靠 `~/.baton/slots/` 下的檔案鎖計數；程序結束或當掉時鎖會自動釋放。沒有空位時每 2 秒重試一次，等待時間算進該次呼叫的 `timeout_secs`，等到逾時就回錯誤，訊息會寫明 CLI 沒有啟動、沒有耗用 token。等了多久記在 run log 的狀態列（`slot wait`）。額度探針不佔位。

### Jev 派工顧問（選用，預設關閉）

開啟後，baton 在啟動 Codex 前，會把 Claude 寫的交辦內容送到 TypeSafe 的 Jev（`https://api.typesafe.ai/v1/systemone`）做判斷。baton 不會主動讀 repo 檔案送出，但交辦內容會整段送出（最多 `max_state_chars` 字元）；交辦裡貼的程式碼、log、設計說明都會一起送到 TypeSafe 位於美國的伺服器。送出前會遮罩常見的憑證格式：PEM 私鑰、`Bearer`／`Authorization:`／`Cookie:` 的值、`sk-`／`ghp_`／`github_pat_`／`glpat-`／`AKIA`／`AIza` 開頭的 key、JWT、`xoxa-`／`xoxb-`／`xoxp-`／`xoxr-`／`xoxs-` 開頭的 Slack token，以及 40 字元以上的長字串。鍵名以 `password`／`passwd`／`pwd`／`secret`／`token`／`api_key`／`apikey`／`api-key`／`access_key`／`private_key`／`client_secret`／`auth`／`credential`／`credentials` 結尾（不分大小寫），後接 `=` 或 `:` 時，或作為 `--password value` 這類旗標並以空格／Tab 接值時，也會保留鍵名、遮罩值；支援引號與 JSON 格式，值即使只有一個字元也會遮罩。`scheme://user:password@host` URL userinfo 中的密碼也會遮罩。

- **難度**：`delegate` 的 `kind = "code"`，以及 `ask_codex` 解析到 frontier 模型（`gpt-6-astra`）時，問 Jev 這件工作是機械小改、日常實作，還是困難工作。`enforce` 模式下，Claude 選了 `hard`、Jev 判為困難的機率又低於 `difficulty_downgrade_below`（預設 0.15）時，改用 `normal` 的模型與推理深度。交辦內容有一行以 `HARD-REASON:` 開頭就不降級。只會降、不會升，`easy`／`normal` 不動。
- **安全審查**：送 Daybreak（`gpt-daybreak-*`）時，`enforce` 模式只套用一條固定規則：推理深度 `xhigh`／`max` 降到 `high`。另外會比對敏感路徑，並問 Jev 這是不是窄範圍重審，但**這個答案只記錄、永遠不套用**。Jev 的回應來自外部，不讓它決定降低安全審查。

`shadow` 模式只記錄，模型與推理深度完全不變，建議先跑一週再決定要不要改成 `enforce`。`enforce` 實際改了設定時，回覆開頭會出現 `[jev: hard→normal p2=…]` 或 `[jev: effort xhigh→high]`。

API key 優先讀環境變數 `TYPESAFE_API_KEY`，其次讀 macOS 鑰匙圈 service `typesafe-api`、account `jev`（`security add-generic-password -U -s typesafe-api -a jev -w`）。key 只在 MCP server 程序內快取，新增 key 後要重開 MCP server（`/mcp` reconnect）；`TYPESAFE_API_KEY` 不會傳給 Codex、Grok、接手 worker、git 或其他 helper 子程序。請求不跟隨轉址、不走代理伺服器。工作目錄解析成實際路徑後，含 `exclude_path_substrings`（預設 `commercial`，不分大小寫）的派工完全不呼叫 Jev。沒有 key、逾時（`timeout_ms`，預設 4000，讀鑰匙圈與 git 都算在內）、HTTP 錯誤、回應超過 64 KiB 或格式不對、git 查詢失敗時，一律照原本的模型與推理深度執行。

每次判斷在 `~/.baton/jev.jsonl` 記一行：時間、對應的 run log 檔名、模式、原本與最後的模型／推理深度、Jev 的機率、是否會改／是否已改、命中的敏感路徑、延遲、input token 數、錯誤碼。不記交辦內容與 key。run log 也多一行 `- jev: …`。

### 設定檔

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
auto_reset = true      # Codex 用完時自動花重置券，無法撤回；false 停用
reset_min_wait_secs = 3600 # 距自然重置不到這麼多秒時保留券

[concurrency]          # 同時執行的 CLI 上限，跨所有 baton 程序；0 表示不限
codex = 4
grok = 4
claude = 2

[claude]               # Claude worker，見上方說明
account = ""           # 留空表示關閉

[jev]                  # Jev 派工顧問，見上方說明
enabled = false
mode = "shadow"        # shadow：只記錄；enforce：依規則降級
model = "jev-latest"
timeout_ms = 4000
max_state_chars = 24000        # 交辦內容超過時保留開頭 2/3 與結尾 1/3
difficulty_downgrade_below = 0.15
rereview_low_effort_above = 0.8
exclude_path_substrings = ["commercial"]
sensitive_path_patterns = ["auth", "fs_safe", "sandbox", "crypto", "secret", "credential", "token", "oauth", "permission", "approval", "acl", "keychain", "redact", "signature", "inject"]

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
- 條款（2026-11-07 查證原文）：[AUP](https://www.anthropic.com/legal/aup)（2026-11-12 生效版）的「Do Not Abuse Our Platform」一節禁止「Coordinate malicious activity across multiple accounts, or create multiple accounts, to avoid detection, circumvent product guardrails, or generate identical or similar inputs that otherwise violate our Usage Policy」。這條針對的是為了逃避偵測、繞過防護，或重送違規內容而使用多個帳號，AUP 和 [Consumer Terms](https://www.anthropic.com/legal/consumer-terms) 都沒有直接提到用量上限。有解讀空間的是同一節的「Intentionally bypass or interfere with capabilities, restrictions, or guardrails established within our products」：用量上限算不算這裡的 restrictions，條文沒有寫明。Consumer Terms 也允許 Anthropic 在認定違約時不經通知停權。baton 不會自動換掉你正在對話的帳號；要不要把第二個帳號設成 worker，由你自己判斷，有疑問可寫信到 support@anthropic.com 確認。

### 資料存放

`~/.baton` 權限 0700，裡面的檔案 0600。內容包括呼叫紀錄（`runs/`）、交接單與報告（`handoffs/`）、各家額度狀態（`quota.json`）、重置券兌換嘗試（`resets.jsonl`，含時間、券 ID、冪等鍵，每次嘗試各有一行 `pending` 與最終結果；只有 `pending` 表示結果未知）、hook 事件紀錄（`events.jsonl`，用來比對實際撞額度時 Claude Code 送出的訊息）、並行上限的鎖檔（`slots/`）、Jev 判斷紀錄（`jev.jsonl`）。

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
