# notes

## backend
- there are multiple tokio runtimes being created in the backend. need to figure out what the best strategy is here. do I want a runtime per thread, or a single runtime for the entire backend? or no runtimes specified in the backend and let the application create and manage the runtime? what is most portable to WASM and other targets?
- why is `sync_transmission!` a macro?

## api
- version numbers on everything. semver api and client versions included in client and server hello messages. Client versions can then be added to server state so every client knows what version other clients are running.


## anylitics
- audio pipeline keeps counters for frames dropped in various stages, mic capture buffer, jitter buffer overflows, anything that might indicate client or network performance issues.
  - need to be carefull about when various counters are initialized and reset so audio stopping and starting doesn't delete useful data. maybe make the counters part of the audio state that persists across stops and starts.
- new backend anylitics task periodically reads backend state and sends anylitics stats to server in anylitics messages.
- server logs anylitics counters per user session, collecting things like time and session length with the various stats from the client.
- server saves anylitics data to the database for later analysis.

## UI
- ensure settings are bing generated dynamicaly
- add a `pin` toggle to each setting (in a right click menu) to pin the setting to the main window to access it outside of the settings window
- rework settings window to have a sidebar with categories and a main panel with the settings for the selected category
  - add scrollbars so the settings fit in the default window size