import { describe, expect, it } from "vitest"
import { UI_LOCALES, UI_STRINGS } from "./strings"
import { NOTICE_TAGS } from "@/lib/sync-api"

const REQUIRED_METRIC_KEYS = [
  "tools.metrics.title",
  "tools.metrics.cpu",
  "tools.metrics.cpuSystem",
  "tools.metrics.cpuUser",
  "tools.metrics.cpuIdle",
  "tools.metrics.gpu",
  "tools.metrics.memory",
  "tools.metrics.storage",
  "tools.metrics.temperature",
  "tools.metrics.battery",
  "tools.metrics.charging",
  "tools.metrics.onBattery",
  "tools.metrics.pluggedIn",
  "tools.metrics.notInstalled",
  "tools.metrics.powerSource",
  "tools.metrics.maxCapacity",
  "tools.metrics.cycleCount",
  "tools.metrics.batteryTemperature",
  "tools.metrics.network",
  "tools.metrics.localIp",
  "tools.metrics.upload",
  "tools.metrics.download",
  "tools.metrics.warming",
  "tools.metrics.unavailable",
  "tools.metrics.error",
  "tools.metrics.stale",
  "tools.metrics.retry",
  "tools.metrics.updated",
  "tools.metrics.loading",
  "tools.metrics.refreshing",
  "tools.metrics.temperatureUnavailable",
  "tools.metrics.seconds",
  "tools.metrics.listenerError",
] as const

const REQUIRED_SPEED_KEYS = [
  "title", "intro", "dataNote", "start", "again", "cancel", "running", "measuredAt",
  "download", "upload", "latency", "dataUsed", "connection", "server", "unknown", "previous",
  "error.busy", "error.cancelled", "error.timeout", "error.network_error", "error.unsupported", "error.failed",
].map((key) => `tools.speed.${key}`)

describe("internet speed translations", () => {
  it("has every key, readable and with the time token, in all eight locales", () => {
    for (const locale of UI_LOCALES) {
      const strings = UI_STRINGS[locale]
      for (const key of REQUIRED_SPEED_KEYS) {
        const value = strings[key]
        expect(value?.trim(), `${locale}:${key}`).toBeTruthy()
        expect(value, `${locale}:${key}`).not.toMatch(/tools\.|speed_/)
      }
      expect(strings["tools.speed.measuredAt"], locale).toContain("{time}")
      expect(strings["tools.speed.intro"], locale).toContain("networkQuality")
      expect(strings["tools.speed.dataNote"], locale).toMatch(/MB|Mo/)
      // The gauges show the progress; the label must not promise a wait.
      expect(strings["tools.speed.running"], locale).not.toMatch(/\d/)
      expect(strings["tools.tabs.speed"], locale).toBeUndefined()
    }
  })
})

describe("system metric translations", () => {
  it("has the explicit nonempty contract in every locale", () => {
    for (const locale of UI_LOCALES) {
      const strings = UI_STRINGS[locale]
      for (const key of REQUIRED_METRIC_KEYS) {
        expect(strings[key], `${locale}:${key}`).toBeTruthy()
      }
      expect(strings["tools.metrics.updated"]).toContain("{age}")
      expect(strings["tools.metrics.seconds"]).toContain("{count}")
      expect(Object.keys(strings).filter((key) => key.startsWith("tools.metrics.")).sort()).toEqual(
        [...REQUIRED_METRIC_KEYS].sort(),
      )
    }
  })
})
describe("display power copy", () => {
  it("describes WindowServer disconnect, not DDC standby, in every locale", () => {
    for (const locale of UI_LOCALES) {
      const note = UI_STRINGS[locale]["tools.brightness.ddcNote"]
      expect(note, `${locale}:tools.brightness.ddcNote`).toBeTruthy()
      expect(note.toLowerCase()).toContain("lunar")
    }
  })
})
describe("tray quit label", () => {
  it("exists in every locale, because the tray's right-click menu shows it", () => {
    for (const locale of UI_LOCALES) {
      const quit = UI_STRINGS[locale]["tray.quit"]
      expect(quit, `${locale}:tray.quit`).toBeTruthy()
    }
  })
})
describe("about page links", () => {
  it("labels every external account link in every locale", () => {
    for (const locale of UI_LOCALES) {
      for (const key of ["settings.about.repo", "settings.about.openrouter", "settings.about.deepl"]) {
        expect(UI_STRINGS[locale][key], `${locale}:${key}`).toBeTruthy()
      }
    }
  })
})
describe("system monitor copy", () => {
  it("names every readout and alert, and carries the reading into each notification", () => {
    const kinds = ["cpu", "memory", "temperature", "storage", "battery"]
    const readouts = ["off", "cpu", "memory", "gpu", "temperature"]
    for (const locale of UI_LOCALES) {
      const strings = UI_STRINGS[locale]
      for (const key of [
        "settings.section.monitor",
        "monitor.readout.label",
        "monitor.readout.desc",
        "monitor.readout.macOnly",
        "monitor.alerts.label",
        "monitor.alerts.desc",
        ...readouts.map((r) => `monitor.readout.${r}`),
        ...kinds.flatMap((k) => [`monitor.alert.${k}.label`, `monitor.alert.${k}.hint`, `monitor.alert.${k}.title`]),
      ]) {
        expect(strings[key], `${locale}:${key}`).toBeTruthy()
      }
      for (const kind of kinds) {
        // The Rust side fills in the reading; a body without the slot would
        // send an alert that never says how bad it is.
        expect(strings[`monitor.alert.${kind}.body`], `${locale}:${kind}`).toContain("{value}")
      }
    }
  })

  it("has the activity panel strings in every locale", () => {
    for (const locale of UI_LOCALES) {
      const strings = UI_STRINGS[locale]
      expect(strings["tools.activity.history"], locale).toContain("{minutes}")
      for (const key of ["historyEmpty", "topCpu", "topMemory", "measuring", "none", "cpuNote"]) {
        expect(strings[`tools.activity.${key}`], `${locale}:${key}`).toBeTruthy()
      }
    }
  })
})

// Frozen cleanup copy contract: keep this list independent of the translations.
const REQUIRED_MOLE_KEYS = [
  "title",
  "intro",
  "detecting",
  "missing",
  "unsupported",
  "failed",
  "running",
  "cleanResult",
  "cleanedCount",
  "cleanedNone",
  "cleanedAll",
  "afterClean",
  "fdaMissing",
  "fdaOpen",
  "fdaRestart",
  "progress",
  "scanning",
  "waiting",
  "empty",
  "stale",
  "updated",
  "technical",
  "expected",
  "partialEstimate",
  "reported",
  "unknownSize",
  "lastRun",
  "items",
  "manual",
  "cancel",
  "confirm",
  "retry",
  "back",
  "moreTools",
  "session.disk",
  "session.diskHint",
  "diskOverlap",
  "session.cache",
  "session.cacheHint",
  "cleanNow",
  "cleanConfirm",
  "cleanWarning",
  "previewWarning",
  "session.tune",
  "session.tuneHint",
  "optimizeNow",
  "optimizeConfirm",
  "optimizeWarning",
  "adminNotice",
  "appsHeading",
  "appsHint",
  "searchApps",
  "noApps",
  "reviewApp",
  "removeHeading",
  "resolvedPath",
  "shortcutNotice",
  "relatedHint",
  "noneRelated",
  "dataWarning",
  "runningNotice",
  "adminNoticeRemoval",
  "scopeWarning",
  "previewExpired",
  "removeSelected",
  "removing",
  "removeDone",
  "removePartial",
  "removeFailed",
  "selectionCount",
  "selectedSize",
  "includesUnknown",
  "excluded",
  ...["preview", "completed", "unchanged", "skipped", "admin_skipped", "manual", "failed", "unknown", "moved", "not_attempted"].map(
    (status) => `status.${status}`,
  ),
  ...["app", "shortcut", "cache", "preferences", "saved_state", "webkit", "http_storage", "support", "container"].map(
    (kind) => `kind.${kind}`,
  ),
  ...[
    "busy",
    "not_installed",
    "unsupported_version",
    "inventory_invalid",
    "inventory_stale",
    "preview_stale",
    "invalid_selection",
    "app_changed",
    "app_running",
    "app_running_unverified",
    "self_app",
    "protected_app",
    "path_unsupported",
    "permission_required",
    "identity_unavailable",
    "shared_data",
    "size_unknown",
    "trash_failed",
    "result_unknown",
    "cancelled",
    "shutting_down",
    "scan_required",
  ].map((error) => `error.mole_${error}`),
  ...[
    "User essentials",
    "App caches",
    "Browsers",
    "Cloud & Office",
    "Developer tools",
    "Applications",
    "Virtualization",
    "Application Support",
    "App leftovers",
    "Apple Silicon updates",
    "Device backups & firmware",
    "Time Machine",
    "Large files",
    "System Data clues",
    "Project artifacts",
  ].map((section) => `section.${section}`),
  ...["User app cache", "User app logs", "Trash"].map((item) => `item.${item}`),
  ...[
    "dnsSpotlight",
    "finderCache",
    "appState",
    "brokenConfig",
    "networkCache",
    "database",
    "launchServices",
    "fonts",
    "dock",
    "finderMetadata",
    "memory",
    "networkStack",
    "permissions",
    "bluetooth",
    "spotlight",
    "periodic",
    "sharedLists",
    "diskHealth",
    "loginItems",
    "quarantine",
    "launchAgents",
    "notifications",
    "usageData",
  ].flatMap((task) => [`task.${task}.title`, `task.${task}.hint`]),
].map((key) => `tools.mole.${key}`)

describe("Mac cleanup translations", () => {
  it("has exactly the approved keys in all eight product locales, without obsolete aliases", () => {
    expect([...UI_LOCALES].sort()).toEqual(["de", "en", "es", "fr", "ja", "ko", "vi", "zh"])
    expect(REQUIRED_MOLE_KEYS).toHaveLength(178)
    for (const locale of UI_LOCALES) {
      const keys = Object.keys(UI_STRINGS[locale]).filter((key) => key.startsWith("tools.mole."))
      expect(keys.sort(), locale).toEqual([...REQUIRED_MOLE_KEYS].sort())
    }
  })

  it("preserves the exact interpolation tokens, including the absence of unexpected tokens", () => {
    const placeholders: Record<string, string[]> = {
      "tools.mole.unsupported": ["{found}", "{required}"],
      "tools.mole.updated": ["{time}"],
      "tools.mole.removeHeading": ["{name}"],
      "tools.mole.selectionCount": ["{count}"],
      "tools.mole.cleanedCount": ["{count}"],
      "tools.mole.error.mole_app_running_unverified": ["{pid}"],
    }
    for (const locale of UI_LOCALES) {
      for (const key of REQUIRED_MOLE_KEYS) {
        const value = UI_STRINGS[locale][key]
        expect((value.match(/\{[^{}]*\}/g) ?? []).sort(), `${locale}:${key}`).toEqual(
          placeholders[key] ?? [],
        )
      }
    }
  })

  it("uses readable copy rather than empty values, translation keys or raw error identifiers", () => {
    for (const locale of UI_LOCALES) {
      for (const key of REQUIRED_MOLE_KEYS) {
        const value = UI_STRINGS[locale][key]
        expect(value.trim(), `${locale}:${key}`).not.toBe("")
        expect(value, `${locale}:${key}`).not.toMatch(/tools\.mole\.|^mole_[a-z_]+$|^(TODO|TBD)$/)
        if (locale !== "ko") {
          expect(value, `${locale}:${key}`).not.toMatch(/[가-힣]/)
        }
      }
      const strings = UI_STRINGS[locale]
      expect(strings["tools.mole.status.unknown"], locale).not.toBe(strings["tools.mole.status.completed"])
      expect(strings["tools.mole.removePartial"], locale).not.toBe(strings["tools.mole.removeDone"])
      expect(strings["tools.mole.expected"], locale).not.toBe(strings["tools.mole.reported"])
      expect(strings["tools.mole.status.admin_skipped"], locale).not.toBe(strings["tools.mole.status.completed"])
      expect(strings["tools.mole.status.manual"], locale).not.toBe(strings["tools.mole.status.completed"])
      expect(strings["tools.mole.selectedSize"], locale).not.toBe(strings["tools.mole.reported"])
    }
  })

  it("retains localized safety statements about previews, opt-in data, and no permanent-delete fallback", () => {
    const safetyCopy = {
      ko: ["이미 확보한 공간이 아닙니다", "기본으로 선택된 관련 파일은 없습니다", "영구삭제로 전환하지 않습니다"],
      en: ["not space already freed", "No related files are selected by default", "Permanent deletion will not be used instead"],
      ja: ["すでに空いた容量ではありません", "初期状態では関連ファイルは選択されていません", "完全削除には切り替えません"],
      zh: ["不代表已释放的空间", "默认不勾选任何相关文件", "不会改为永久删除"],
      es: ["no es espacio ya liberado", "Ninguno está seleccionado por defecto", "No se recurrirá a la eliminación permanente"],
      fr: ["n’est pas de l’espace déjà libéré", "Aucun n’est sélectionné par défaut", "Il ne sera pas remplacé par une suppression définitive"],
      de: ["keinen bereits freigegebenen Speicherplatz", "Standardmäßig ist keine ausgewählt", "Es wird nicht stattdessen endgültig gelöscht"],
      vi: ["không phải dung lượng đã được giải phóng", "Mặc định không chọn tệp liên quan nào", "Không chuyển sang xóa vĩnh viễn"],
    }
    for (const locale of UI_LOCALES) {
      const strings = UI_STRINGS[locale]
      const [preview, related, trash] = safetyCopy[locale]
      expect(strings["tools.mole.previewWarning"], locale).toContain(preview)
      expect(strings["tools.mole.relatedHint"], locale).toContain(related)
      expect(strings["tools.mole.error.mole_trash_failed"], locale).toContain(trash)
      for (const name of ["Dock", "Finder", "USB", "Bluetooth"]) {
        expect(strings["tools.mole.optimizeWarning"], `${locale}:${name}`).toContain(name)
      }
      expect(strings["tools.mole.scopeWarning"], locale).toContain("Homebrew")
      expect(strings["tools.mole.task.finderMetadata.hint"], locale).toContain(".DS_Store")
    }
  })

  it("preserves the reviewed Korean safety wording exactly", () => {
    const expected = {
      "tools.mole.session.tune": "기록·설정 정리",
      "tools.mole.session.tuneHint": "다운로드·알림·사용 기록과 오래된 앱 상태, 손상된 설정을 정리하고 Finder·Dock 캐시와 파일 연결 정보를 새로 만듭니다. 실행 중인 앱을 종료하거나 메모리를 비우지 않습니다.",
      "tools.mole.optimizeNow": "기록·설정 정리…",
      "tools.mole.optimizeConfirm": "Mole의 전체 기록·설정 정리를 실행할까요?",
      "tools.mole.selectedSize": "선택한 항목의 크기",
      "tools.mole.includesUnknown": "일부 크기 미확인 · 확인된 크기만 합산",
      "tools.mole.status.admin_skipped": "관리자 권한 작업 제외",
      "tools.mole.status.manual": "직접 확인 필요",
      "tools.mole.optimizeWarning": "오래된 앱 상태·손상된 설정·다운로드·알림·사용 기록이 삭제될 수 있으며, 일부 데이터베이스와 네트워크·USB 저장장치의 Finder 설정이 변경됩니다. Dock·알림 센터가 재시작될 수 있습니다. 사용 기록 DB의 WAL 삭제로 기록이 손실될 수 있습니다. 메모리 해제와 네트워크·Bluetooth 재시작 등 관리자 작업은 실행하지 않으며, 개별 작업 선택 실행은 아닙니다.",
    }
    for (const [key, value] of Object.entries(expected)) {
      expect(UI_STRINGS.ko[key], key).toBe(value)
    }
  })

  it("explains partial totals, excluded administrator tasks and history loss in every locale", () => {
    const safetyCopy = {
      ko: ["일부 크기 미확인", "확인된 크기만 합산", "관리자 권한 작업 제외", "직접 확인 필요", "관리자 작업은 실행하지 않으며"],
      en: ["Some sizes unknown", "Only known sizes are totaled", "Administrator tasks excluded", "Manual review required", "restarts are not performed"],
      ja: ["一部のサイズが未確認", "確認済みのサイズのみ合計", "管理者権限が必要な作業を除外", "手動での確認が必要", "管理者権限が必要な作業は行いません"],
      zh: ["部分大小未知", "仅合计已确认的大小", "已排除需管理员权限的操作", "需手动检查", "不执行内存释放"],
      es: ["Algunos tamaños desconocidos", "Solo se suman los tamaños conocidos", "Tareas de administrador excluidas", "Revisión manual necesaria", "No se ejecutan tareas de administrador"],
      fr: ["Certaines tailles inconnues", "Seules les tailles connues sont additionnées", "Tâches d’administration exclues", "Vérification manuelle nécessaire", "ne sont pas exécutées"],
      de: ["Einige Größen unbekannt", "Nur bekannte Größen werden addiert", "Aufgaben mit Administratorrechten ausgeschlossen", "Manuelle Prüfung erforderlich", "werden nicht ausgeführt"],
      vi: ["Một số kích thước chưa rõ", "Chỉ cộng các kích thước đã xác định", "Đã loại trừ tác vụ cần quyền quản trị viên", "Cần kiểm tra thủ công", "Không chạy tác vụ quản trị"],
    }
    for (const locale of UI_LOCALES) {
      const strings = UI_STRINGS[locale]
      const [unknown, total, admin, manual, excluded] = safetyCopy[locale]
      expect(strings["tools.mole.includesUnknown"], locale).toContain(unknown)
      expect(strings["tools.mole.includesUnknown"], locale).toContain(total)
      expect(strings["tools.mole.status.admin_skipped"], locale).toBe(admin)
      expect(strings["tools.mole.status.manual"], locale).toBe(manual)
      expect(strings["tools.mole.optimizeWarning"], locale).toContain(excluded)
      expect(strings["tools.mole.optimizeWarning"], locale).toContain("WAL")
      for (const detail of ["100MiB", "90", "WAL", "SHM"]) {
        expect(strings["tools.mole.task.usageData.hint"], `${locale}:${detail}`).toContain(detail)
      }
    }
  })
  it("does not advertise purge as an available memory-release capability in any locale", () => {
    const unavailable = {
      ko: "이 앱에서 실행하지 않습니다",
      en: "not performed by this app",
      ja: "このアプリでは行いません",
      zh: "本应用不执行",
      es: "esta app no lo ejecuta",
      fr: "n’est pas exécutée par cette app",
      de: "wird von dieser App nicht ausgeführt",
      vi: "không được ứng dụng này thực hiện",
    }
    for (const locale of UI_LOCALES) {
      expect(UI_STRINGS[locale]["tools.mole.task.memory.hint"], locale).toContain("purge")
      expect(UI_STRINGS[locale]["tools.mole.task.memory.hint"], locale).toContain(unavailable[locale])
    }
  })
})

// G9 sync settings: the exact key set, identical in every locale, no empty
// value, no raw identifier leaking into copy, and the AC 30 revocation limit.
describe("sync settings translations", () => {
  const syncKeys = (locale: (typeof UI_LOCALES)[number]) =>
    Object.keys(UI_STRINGS[locale]).filter((k) => k === "settings.section.sync" || k.startsWith("settings.sync.")).sort()
  /** Keys added by the G9 pass-1 fixes (U1-U6); pinned so a locale cannot lose one silently. */
  const PASS1_KEYS = [
    "settings.sync.connect.reconsentTitle",
    "settings.sync.connect.reconsentDesc",
    "settings.sync.restored.line",
    "settings.sync.restored.kind.memo",
    "settings.sync.restored.kind.conv",
    "settings.sync.restored.kind.tr",
    "settings.sync.restored.kind.policy",
    "settings.sync.restored.kind.other",
    "settings.sync.notice.unknown",
    "settings.sync.advanced.devices",
    "settings.sync.advanced.noDevices",
    "settings.sync.advanced.thisDevice",
    "settings.sync.advanced.removedDevice",
    "settings.sync.advanced.lastSeen",
    "settings.sync.advanced.neverSeen",
    "settings.sync.advanced.removeLimit",
  ]

  it("has the same non-empty key set in all eight locales", () => {
    const expected = syncKeys("ko")
    expect(expected.length).toBeGreaterThan(150)
    for (const locale of UI_LOCALES) {
      expect(syncKeys(locale), locale).toEqual(expected)
      for (const key of expected) {
        const value = UI_STRINGS[locale][key]
        expect(value?.trim(), `${locale}:${key}`).toBeTruthy()
        // Copy never shows a key path or a snake_case code.
        expect(value, `${locale}:${key}`).not.toMatch(/settings\.sync|\b[a-z]+_[a-z_]+\b/)
      }
    }
  })

  it("covers every engine notice variant, the pass-1 keys and the dialogs' placeholders", () => {
    for (const locale of UI_LOCALES) {
      const strings = UI_STRINGS[locale]
      for (const tag of NOTICE_TAGS) expect(strings[`settings.sync.notice.${tag}`], `${locale}:${tag}`).toBeTruthy()
      for (const key of PASS1_KEYS) expect(strings[key]?.trim(), `${locale}:${key}`).toBeTruthy()
      // U4: the free-text device id is gone with the picker.
      expect(strings["settings.sync.advanced.deviceId"], locale).toBeUndefined()
      expect(strings["settings.sync.notice.other_vaults"], locale).toContain("{count}")
      expect(strings["settings.sync.notice.manifest_copies"], locale).toContain("{count}")
      expect(strings["settings.sync.restored.line"], locale).toContain("{count}")
      expect(strings["settings.sync.advanced.removeLimit"], locale).toContain("{device}")
      expect(strings["settings.sync.advanced.lastSeen"], locale).toContain("{when}")
      expect(strings["settings.sync.vault.retry"], locale).toContain("{seconds}")
      expect(strings["settings.sync.disconnect.typeWord"], locale).toContain("{word}")
      for (const key of ["skip", "republish", "copy", "evicted", "done"]) {
        expect(strings[`settings.sync.join.${key}`], `${locale}:${key}`).toContain("{count}")
      }
      for (const slot of ["{up}", "{down}", "{cycles}", "{quarantined}"]) {
        expect(strings["settings.sync.status.countsLine"], `${locale}:${slot}`).toContain(slot)
      }
    }
  })

  it("U1: the RK dialog says the key is stored offline and this app does not keep it, in every locale", () => {
    expect(UI_STRINGS.en["settings.sync.rk.showDesc"]).toBe(
      "This key opens the vault. It is shown once, now. Store it offline; this app does not keep it.",
    )
    const parts = {
      ko: ["오프라인에 보관", "보관하지 않습니다"],
      en: ["offline", "does not keep it"],
      ja: ["オフラインで保管", "保持しません"],
      zh: ["离线保存", "不会保留"],
      es: ["fuera de línea", "no la conserva"],
      fr: ["hors ligne", "ne la garde pas"],
      de: ["offline", "behält ihn nicht"],
      vi: ["ngoại tuyến", "không giữ khóa"],
    }
    for (const locale of UI_LOCALES) {
      const text = UI_STRINGS[locale]["settings.sync.rk.showDesc"]
      for (const part of parts[locale]) expect(text, `${locale}:${part}`).toContain(part)
    }
  })

  it("U4: the remove-device copy states that a removed device can still read the vault (AC 30)", () => {
    const parts = {
      ko: "계속 읽을 수 있습니다",
      en: "can still read the vault",
      ja: "読むことはできます",
      zh: "仍可读取保管库",
      es: "aún puede leer la bóveda",
      fr: "peut encore lire le coffre",
      de: "weiterhin lesen",
      vi: "vẫn đọc được kho",
    }
    for (const locale of UI_LOCALES) {
      for (const key of ["settings.sync.advanced.removeDeviceDesc", "settings.sync.advanced.removeLimit"]) {
        expect(UI_STRINGS[locale][key], `${locale}:${key}`).toContain(parts[locale])
      }
    }
  })

  it("keeps the AC 30 revocation-limit wording and the D-1 wrong-key line", () => {
    expect(UI_STRINGS.en["settings.sync.advanced.revocationLimit"]).toBe(
      "Reissuing the key stops the old key from adding new devices. It does not lock out devices that are already connected, and it does not erase data they already have.",
    )
    expect(UI_STRINGS.en["settings.sync.join.wrongKey"]).toBe("This key doesn't open any vault in this storage.")
    expect(UI_STRINGS.en["settings.sync.migration.deferred"]).toBe("Moving your existing data will retry at next launch.")
    // Every locale states both halves of the limit: no lock-out, no erasure.
    const limit = {
      ko: ["차단되지 않고", "지워지지 않습니다"],
      en: ["does not lock out", "does not erase"],
      ja: ["締め出されず", "消えません"],
      zh: ["不会锁住", "不会清除"],
      es: ["No bloquea", "ni borra"],
      fr: ["ne bloque pas", "n’efface pas"],
      de: ["nicht ausgesperrt", "nicht gelöscht"],
      vi: ["không chặn", "không xóa"],
    }
    for (const locale of UI_LOCALES) {
      const text = UI_STRINGS[locale]["settings.sync.advanced.revocationLimit"]
      for (const part of limit[locale]) expect(text, `${locale}:${part}`).toContain(part)
    }
  })
})
