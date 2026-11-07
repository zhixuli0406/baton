# Changelog

格式依 [Keep a Changelog 1.1.0](https://keepachangelog.com/zh-TW/1.1.0/)，版本號依 [SemVer](https://semver.org/lang/zh-TW/)。

## [Unreleased]

### Added
- Jev 派工顧問（`[jev]`，預設關閉）：啟動 Codex 前把交辦內容（先遮罩常見憑證格式）送 TypeSafe Jev 判斷難度。`shadow` 只記錄；`enforce` 在 Jev 判為困難機率低於 0.15 時把 `hard` 降為 `normal`（`HARD-REASON:` 可覆蓋）。Daybreak 的 `xhigh`／`max` 在 `enforce` 降為 `high`；Jev 對安全重審的判斷只記錄、不套用。任何失敗照原設定執行；判斷記在 `~/.baton/jev.jsonl` 與 run log。新增依賴 `reqwest`（rustls，無預設功能、不走代理）。
- 並行上限：`[concurrency]` 設定 `codex`、`grok` 同時執行的 CLI 數量，預設各 4，`0` 不限。上限跨所有 baton 程序共用（`~/.baton/slots/` 檔案鎖，程序結束自動釋放）；等待時間算進呼叫的 timeout，等到逾時回錯誤且不啟動 CLI。run log 狀態列多記 `slot wait`。
- Codex 額度重置券自動兌換：`delegate`、`ask_codex` 與接手 worker 遇到可恢復的額度上限時，預設自動花一張券，無法撤回；`[rotation] auto_reset = false` 可停用。`reset_min_wait_secs` 預設 3600，距自然重置不到這麼多秒就保留券。跨程序鎖與固定冪等鍵防止重複兌換，每個工作跨步驟與重試最多兌換一張券。嘗試會在送出前寫一行 `pending`，結束後再寫一行最終結果到 `~/.baton/resets.jsonl`，兩行共用券 ID 與冪等鍵；只有 `pending` 而沒有最終結果表示結果未知。取消呼叫後，已啟動的兌換仍會完成並記錄結果。`baton quota` 與 `quota_status` 永遠只查詢、不花券；額度輸出會列出可用張數。呼叫中恢復額度後，`delegate` 與接手 worker 僅在工作樹未改動時重試同一步一次，`ask_codex` 則提示重新呼叫。
- 額度探針：`baton quota [--refresh]` 與 MCP 工具 `quota_status`。Codex 透過 `codex app-server` 的 `account/rateLimits/read` 即時查詢（不耗額度，同一個 MCP server 內的並行呼叫共用一次探測），Grok 沒有額度 API，只在呼叫因額度失敗後標為用完。狀態存於 `~/.baton/quota.json`。
- 自動輪替：MCP 工具 `delegate` 依 `[rotation] order`（預設 codex → grok）跳過額度用完的廠商，呼叫中撞額度就換下一家；全部用完時回 `DELEGATES_EXHAUSTED`，由 Claude 自己做。模型與推理深度依 `kind`（code／security／research）與 `difficulty`（easy／normal／hard）決定。`write` 模式下 agent 已改檔才撞額度時不換人。額度錯誤只從 CLI stderr 的最後 5 行判斷，避免 agent 讀到的檔案內容造成誤判；`refresh` 會清掉 Grok 的額度錯誤紀錄。
- 授權：MIT OR Apache-2.0 雙授權（`LICENSE-MIT`、`LICENSE-APACHE`）。
- `[rotation]` 設定：`enabled`、`order`、`probe_ttl_secs`、`cooldown_secs`。
- `[write] allowed_roots`：讓 MCP `ask_*` 與 `handoff_start` 的 `write` 模式可跨 repo／worktree，寫入指定根目錄下的 git repo。支援 `~`／`~/` 展開，相對路徑視為設定錯誤；預設為空，保留原有範圍。
- `baton mcp`：stdio MCP server，提供 `ask_codex`、`ask_grok`，支援 `read_only`／`write` 模式、逾時、並行呼叫與取消；每次呼叫記錄到 `~/.baton/runs/`。
- 依能力選模型：`ask_*` 新增 `capability`（`security`／`frontier`／`workhorse`／`fast`／`long_context`／`web_search`／`legacy`）與 `effort`（`low`～`max`）參數；內建模型表取自各家 CLI 的模型目錄（2026-10-27），資安任務預設路由到 Codex `gpt-daybreak-blue-latest`；新增 `list_models` 工具；`~/.baton/config.toml` 可用 `[[catalog]]` 新增或覆蓋模型列。回覆開頭與 run log 標出實際模型與 effort。
- 額度用完自動接手：`StopFailure` hook 偵測訂閱額度上限，產生交接單並在背景啟動接手 agent；接手期間 `PreToolUse` 擋下 Claude 的 Edit/Write/Bash；完成後透過 `PreToolUse`／`UserPromptSubmit` 把報告送回 Claude。
- `baton handoff list|show|stop|start`。
- MCP 工具 `handoff_list`／`handoff_show`／`handoff_stop`／`handoff_start`：對應上述四個 CLI 動作，讓 Claude 在對話中查看接手進度、取回任務或手動交接。`handoff_show` 的報告框在 `<handoff_report>` 內且不記為已送達；`handoff_start` 的 `write` 模式沿用 `ask_*` 的專案範圍限制；由工具啟動的交接 `trigger` 記為 `mcp`。
- `baton account add|list|remove|sync`、`baton run`、`baton switch`：每帳號一個 `CLAUDE_CONFIG_DIR`，以 symlink 共用設定與對話紀錄；手動切換。
- `baton install [--apply]`：註冊 hooks（先備份 settings.json，換位置時就地更新）與 MCP server，並合併 MCP 設定到各帳號。

### Changed
- baton 啟動的 `codex exec` 一律加上 `-c features.multi_agent=false -c features.memories=false`，不讓 Codex 在派工中自開子代理、不載入 memories（實測子代理約占派工 token 的 37%）。需要 Codex 子代理的工作請直接用 Codex。
- 接手的背景 worker 會照 `[rotation] order` 輪替：原定 agent 額度用完時換下一家，換過人會記在 handoff 備註與報告開頭；已改檔才撞額度則不換。
- `ask_codex`／`ask_grok` 在該家額度用完且無法用重置券恢復時回錯誤，呼叫中撞額度會記錄；Codex 兌換後提示重新呼叫，其他情況提示改用 `delegate`。

### Removed
- `ask_gemini`（Google Antigravity CLI `agy`）：Google 官方 FAQ 明寫 consumer 方案下包進第三方 coding agent 不受支援、條款第 6 條視為違約，且停權連帶封鎖 Gemini CLI 與 Code Assist（2026-10-27）。`BATON_GEMINI_BIN` 一併移除；`[handoff] agent = "gemini"` 現在會被設定檔拒絕。

### Security
- agent、git 與其他 helper 子程序不再繼承 `TYPESAFE_API_KEY`。
- Jev 送出前也會遮罩短 key/value 憑證（如 `password=x`、`api_key: abc123`，含引號與 JSON 格式）、`--password value` 旗標值及 URL userinfo 密碼，並遮罩 Slack token；鍵名保留，值即使只有一個字元也會遮罩。
- `write` 模式比對目標與啟動專案的實體 `.git` 邊界，拒絕 `core.worktree` 重定位造成的範圍越界；README 補充啟動環境與目錄替換競態的信任前提。
- MCP `write` 模式要求目標位於 git repo 內，且在 Claude Code 啟動的 repo 內，或目標 repo 根目錄位於 `[write] allowed_roots` 下；範圍比對與啟動 agent 都使用解析後的實際路徑，Git 探測會清除影響 repo 與設定的環境變數並檢查目錄歸屬。家目錄、家目錄的上層目錄與 `/` 不能作為目標 repo、啟動的 repo 或允許的根目錄，不存在的根目錄不授權，目標 repo 接手期間拒絕。
- grok 兩種模式都改用工具白名單（`plan` 模式在 headless 擋不住改檔）；`write` 模式不開放 shell，`read_only` 不開放 `web_fetch`。
- 交接單遮蔽常見金鑰與私鑰；交接資料與接手報告以標記圈起，標示為不可執行的資料；接手 agent 改動 agent／git 設定檔時發出警告。
- `model` 參數限制字元並以 `--model=<id>` 傳遞，避免被當成其他旗標。
- `~/.baton` 改為 0700、檔案 0600。
- worker 存活改用 flock 判斷，不再信任可能被重用的 pid；MCP server 與 worker 收到 SIGTERM/SIGINT/SIGHUP 時會一併終止 agent。
