// awb-claude-insets: injected by scripts/patch-claude-desktop.sh at the top of
// Claude Desktop's main script.
//
// Electron's Linux title bar overlay only keeps clear of Electron's own caption
// buttons, so the page's env(titlebar-area-*) ignores the controls Android
// draws into the caption. Under Wayland Bridge this pads the page's title bar
// by Android's caption insets (/run/awb/caption.json, logical px = dp, written
// by the app) and draws Electron's own buttons transparent: they sit under
// Android's.
;(function () {
  try {
    if (process.type !== "browser" || process.env.WAYLAND_DISPLAY !== "/run/awb/wayland-0") return
    const electron = require("electron")
    const { app, BrowserWindow } = electron
    const fs = require("fs")

    // Android's rects end in empty margin (dp): after the app chip's pill,
    // and before the first window control's glyph. The title bar may run
    // into that much of them.
    const START_MARGIN = 10
    const END_MARGIN = 52

    let caption = null
    const load = () => {
      try {
        caption = JSON.parse(fs.readFileSync("/run/awb/caption.json", "utf8"))
      } catch {
        caption = null
      }
    }
    load()

    const setOverlay = BrowserWindow.prototype.setTitleBarOverlay
    BrowserWindow.prototype.setTitleBarOverlay = function (options) {
      return setOverlay.call(this, Object.assign({}, options, { color: "#00000000", symbolColor: "#00000000" }))
    }

    // The page's title bar: its inline padding comes from env(titlebar-area-*)
    // plus gutters. The start padding is replaced; at the end the gutter
    // variable takes the inset less Electron's own button area, which the
    // page's padding adds back.
    const cssFor = (wc) => {
      const win = BrowserWindow.fromWebContents(wc)
      const c = caption && caption[win && win.isMaximized() ? "maximized" : "normal"]
      if (!c) return null
      const zoom = wc.getZoomFactor() || 1
      const px = (v) => (Math.max(0, v) / zoom).toFixed(2) + "px"
      const start = px(c.start - START_MARGIN)
      const end = px(c.end - END_MARGIN)
      return (
        ".dframe-chrome-bar{" +
        `padding-left:${start}!important;` +
        `--df-chrome-bar-pad-start:${start}!important;` +
        `--df-chrome-bar-end-gutter:calc(${end} - max(0px, 100% - env(titlebar-area-x, 0px) - env(titlebar-area-width, 100%)))!important}`
      )
    }

    // Inserted CSS per page, replaced when the insets, zoom or window state change.
    const applied = new Map()
    const apply = (wc) => {
      if (wc.isDestroyed()) return
      const css = cssFor(wc)
      const current = applied.get(wc.id)
      if (current && current.css === css) return
      const entry = { css, key: null }
      applied.set(wc.id, entry)
      if (current && current.key) wc.removeInsertedCSS(current.key).catch(() => {})
      if (css) wc.insertCSS(css).then((key) => (entry.key = key), () => {})
    }

    // AWB_DEVTOOLS=1 opens DevTools (detached) on the claude.ai pages: Claude
    // refuses to start with debugging switches on its command line.
    const devtools = process.env.AWB_DEVTOOLS === "1"

    app.on("web-contents-created", (_e, wc) => {
      if (devtools) {
        wc.on("did-finish-load", () => {
          if (/^https:\/\/claude\.ai\//.test(wc.getURL()) && !wc.isDevToolsOpened()) wc.openDevTools({ mode: "detach" })
        })
      }
      wc.on("dom-ready", () => {
        applied.delete(wc.id)
        apply(wc)
      })
      wc.on("destroyed", () => applied.delete(wc.id))
    })
    app.on("browser-window-created", (_e, win) => {
      try {
        win.setTitleBarOverlay({})
      } catch {}
    })
    setInterval(() => {
      load()
      for (const wc of electron.webContents.getAllWebContents()) {
        if (applied.has(wc.id)) apply(wc)
      }
    }, 2000).unref()
  } catch {}
})();
