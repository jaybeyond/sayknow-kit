// What changed in each release, in every language the site speaks. Newest
// first. A test fails when the app's version has no entry here, so a release
// cannot ship without saying what it changed.
import type { Locale } from "./site"

export type ChangelogEntry = {
  version: string
  /** YYYY-MM-DD, the day the release was published. */
  date: string
  notes: Record<Locale, string[]>
}

export const CHANGELOG: ChangelogEntry[] = [
  {
    version: "0.3.9",
    date: "2026-09-29",
    notes: {
      ko: ["Codex 한도를 기존 로그인으로 OpenAI에서 직접 갱신합니다. 오래된 로컬 기록은 더 이상 현재 사용량으로 표시하지 않습니다."],
      en: ["Codex limits now refresh directly from OpenAI using the existing login. Old local readings are no longer presented as current usage."],
      ja: ["Codex の上限を既存のログインで OpenAI から直接更新します。古いローカル記録を現在の使用量として表示しなくなりました。"],
      zh: ["现在使用现有登录信息直接从 OpenAI 更新 Codex 限额，不再将旧的本地记录显示为当前用量。"],
      es: ["Los límites de Codex se actualizan directamente desde OpenAI con la sesión existente. Las lecturas locales antiguas ya no se muestran como uso actual."],
      fr: ["Les quotas Codex sont actualisés directement auprès d'OpenAI avec la session existante. Les anciennes mesures locales ne sont plus présentées comme actuelles."],
      de: ["Codex-Limits werden mit der bestehenden Anmeldung direkt bei OpenAI aktualisiert. Alte lokale Messwerte erscheinen nicht mehr als aktuelle Nutzung."],
      vi: ["Giới hạn Codex được cập nhật trực tiếp từ OpenAI bằng phiên đăng nhập hiện có. Số liệu cục bộ cũ không còn được hiển thị là mức dùng hiện tại."],
    },
  },
  {
    version: "0.3.8",
    date: "2026-09-29",
    notes: {
      ko: [
        "탭·설정 메뉴·번역/재작성 전환·도구 탭의 선택 표시가 미끄러지듯 옮겨 가고, 고정 핀 아이콘과 채팅 아이콘이 부드럽게 바뀝니다.",
        "검색창에 지우기 버튼이 생겼고, 지우면 글자가 흩어지며 사라집니다. 번역 입력창, 클립보드, 기록, 언어 선택에 적용됩니다.",
        "잘못된 키를 입력하면 입력창이 흔들리고, 번역·생각 중·사용량 스캔 표시에 빛이 흐르는 효과가 들어갔습니다.",
        "웹사이트가 추가되었습니다: 기능, 단축키, 다운로드, 변경 내역, 피드백 페이지를 8개 언어로 제공합니다.",
      ],
      en: [
        "The selection highlight in the tabs, the settings menu, the translate/rewrite switch and the Tools tabs now glides to the new choice, and the pin and chat icons cross-fade instead of cutting.",
        "Search boxes gain a clear button, and clearing dissolves the text away. It applies to the translate input, Clipboard, History and the language picker.",
        "A rejected key shakes its field, and the translating, thinking and usage-scan labels get a shimmer sweep.",
        "The website is new: features, shortcuts, download, changelog and feedback pages in eight languages.",
      ],
      ja: [
        "タブ・設定メニュー・翻訳/書き換えの切り替え・ツールのタブで、選択表示が滑らかに移動し、ピンとチャットのアイコンも切り替わりが滑らかになりました。",
        "検索ボックスにクリアボタンが付き、消すと文字が散るように消えます。翻訳入力欄、クリップボード、履歴、言語選択で使えます。",
        "無効なキーを入力すると入力欄が揺れ、翻訳中・考え中・使用量スキャンの表示に光が流れる演出が加わりました。",
        "ウェブサイトを追加しました。機能、ショートカット、ダウンロード、変更履歴、フィードバックのページを8言語で提供します。",
      ],
      zh: [
        "标签页、设置菜单、翻译/改写切换和工具标签页的选中标记现在会平滑滑动，置顶和聊天图标也改为渐变切换。",
        "搜索框新增清除按钮，清除时文字会散开消失。适用于翻译输入框、剪贴板、历史记录和语言选择器。",
        "输入被拒绝的密钥时输入框会抖动，翻译中、思考中和用量扫描的提示加入了流光效果。",
        "新增网站：提供功能、快捷键、下载、更新日志和反馈页面，支持八种语言。",
      ],
      es: [
        "El resaltado de selección en las pestañas, el menú de ajustes, el cambio traducir/reescribir y las pestañas de Herramientas ahora se desliza a la nueva opción, y los iconos de fijar y chat se funden en lugar de cambiar de golpe.",
        "Los cuadros de búsqueda tienen un botón de borrar, y al borrar el texto se disuelve. Se aplica a la entrada de traducción, Portapapeles, Historial y el selector de idioma.",
        "Una clave rechazada sacude el campo, y las etiquetas de traduciendo, pensando y análisis de uso muestran un destello.",
        "Nuevo sitio web: páginas de funciones, atajos, descarga, cambios y comentarios en ocho idiomas.",
      ],
      fr: [
        "La sélection dans les onglets, le menu des réglages, le commutateur traduire/réécrire et les onglets Outils glisse désormais vers le nouveau choix, et les icônes d'épingle et de chat se fondent au lieu de basculer brusquement.",
        "Les champs de recherche ont un bouton d'effacement, et le texte se dissout quand on l'efface. Cela vaut pour la saisie de traduction, le Presse-papiers, l'Historique et le sélecteur de langue.",
        "Une clé refusée fait trembler son champ, et les libellés de traduction, de réflexion et d'analyse d'usage ont un reflet qui les balaie.",
        "Nouveau site web : pages fonctionnalités, raccourcis, téléchargement, journal des modifications et retours, en huit langues.",
      ],
      de: [
        "Die Auswahlmarkierung in den Tabs, im Einstellungsmenü, beim Umschalter Übersetzen/Umschreiben und in den Werkzeug-Tabs gleitet jetzt zur neuen Auswahl, und die Pin- und Chat-Symbole blenden über statt hart zu wechseln.",
        "Suchfelder haben eine Löschen-Schaltfläche, und beim Löschen löst sich der Text auf. Gilt für das Übersetzungsfeld, die Zwischenablage, den Verlauf und die Sprachauswahl.",
        "Ein abgelehnter Schlüssel lässt sein Feld wackeln, und die Anzeigen für Übersetzen, Nachdenken und Nutzungsscan erhalten einen Schimmer.",
        "Neue Website: Seiten zu Funktionen, Tastenkürzeln, Download, Änderungsprotokoll und Feedback in acht Sprachen.",
      ],
      vi: [
        "Vùng chọn ở các tab, menu cài đặt, nút chuyển dịch/viết lại và các tab Công cụ giờ trượt mượt sang lựa chọn mới, còn biểu tượng ghim và trò chuyện chuyển dần thay vì đổi đột ngột.",
        "Ô tìm kiếm có nút xóa, và khi xóa chữ sẽ tan biến. Áp dụng cho ô nhập dịch, Bộ nhớ tạm, Lịch sử và bộ chọn ngôn ngữ.",
        "Khóa bị từ chối sẽ làm ô nhập rung lên, và các nhãn đang dịch, đang nghĩ, quét mức dùng có hiệu ứng ánh sáng lướt qua.",
        "Trang web mới: các trang tính năng, phím tắt, tải về, nhật ký thay đổi và phản hồi bằng tám ngôn ngữ.",
      ],
    },
  },
  {
    version: "0.3.7",
    date: "2026-09-28",
    notes: {
      ko: ["상태 탭 그래프가 고정된 영역 안에 그려지고, 최신 값이 항상 오른쪽 끝에 옵니다. 값이 바뀌어도 그래프가 흔들리지 않습니다."],
      en: ["The Status tab graphs sit in a fixed frame with the newest reading on the right edge, so they no longer shift as values change."],
      ja: ["ステータスタブのグラフが固定枠に描かれ、最新値が常に右端に来ます。値が変わってもグラフが揺れません。"],
      zh: ["状态标签页的图表绘制在固定区域内，最新读数始终在最右侧，数值变化时图表不再晃动。"],
      es: ["Las gráficas de la pestaña Estado tienen un marco fijo y la lectura más reciente siempre a la derecha, así que ya no se desplazan."],
      fr: ["Les graphiques de l'onglet État ont un cadre fixe, la dernière mesure toujours à droite : ils ne bougent plus quand les valeurs changent."],
      de: ["Die Diagramme im Status-Tab haben einen festen Rahmen, der neueste Wert steht immer rechts – sie springen nicht mehr."],
      vi: ["Biểu đồ trong tab Trạng thái nằm trong khung cố định, giá trị mới nhất luôn ở mép phải nên không còn xê dịch."],
    },
  },
  {
    version: "0.3.6",
    date: "2026-09-28",
    notes: {
      ko: [
        "macOS 전역 단축키가 ⌃⌥⌘로 바뀌었습니다: ⌃⌥⌘S 열기/닫기, ⌃⌥⌘1–4 탭으로 바로 열기, ⌃⌥⌘M 새 메모. 다른 앱과 겹치던 ⌃⌥ 조합을 피했습니다.",
        "도구의 사용량 탭 이름이 “토큰 사용량”이 되었습니다.",
      ],
      en: [
        "macOS global shortcuts moved to ⌃⌥⌘: ⌃⌥⌘S to show or hide, ⌃⌥⌘1–4 to open a tab, ⌃⌥⌘M for a new memo. The old ⌃⌥ keys clashed with other apps.",
        "The Usage tab in Tools is now called “Token usage”.",
      ],
      ja: [
        "macOS のグローバルショートカットを ⌃⌥⌘ に変更：⌃⌥⌘S で開閉、⌃⌥⌘1–4 で各タブ、⌃⌥⌘M で新規メモ。他のアプリと重なっていた ⌃⌥ を避けました。",
        "ツールの使用量タブの名前が「トークン使用量」になりました。",
      ],
      zh: [
        "macOS 全局快捷键改为 ⌃⌥⌘：⌃⌥⌘S 打开/关闭，⌃⌥⌘1–4 直接打开对应标签页，⌃⌥⌘M 新建便签。避开了与其他应用冲突的 ⌃⌥。",
        "工具中的用量标签页更名为“Token 用量”。",
      ],
      es: [
        "Los atajos globales de macOS pasan a ⌃⌥⌘: ⌃⌥⌘S para mostrar u ocultar, ⌃⌥⌘1–4 para abrir una pestaña, ⌃⌥⌘M para una nota nueva. Los antiguos ⌃⌥ chocaban con otras apps.",
        "La pestaña Uso de Herramientas ahora se llama “Uso de tokens”.",
      ],
      fr: [
        "Les raccourcis globaux macOS passent à ⌃⌥⌘ : ⌃⌥⌘S pour afficher ou masquer, ⌃⌥⌘1–4 pour ouvrir un onglet, ⌃⌥⌘M pour un nouveau mémo. Les anciens ⌃⌥ entraient en conflit avec d'autres apps.",
        "L'onglet Utilisation des Outils s'appelle désormais « Jetons ».",
      ],
      de: [
        "Globale Kurzbefehle unter macOS jetzt mit ⌃⌥⌘: ⌃⌥⌘S ein-/ausblenden, ⌃⌥⌘1–4 öffnet einen Tab, ⌃⌥⌘M neue Notiz. Die alten ⌃⌥-Kombinationen kollidierten mit anderen Apps.",
        "Der Nutzung-Tab unter Werkzeuge heißt jetzt „Token-Nutzung“.",
      ],
      vi: [
        "Phím tắt toàn cục trên macOS chuyển sang ⌃⌥⌘: ⌃⌥⌘S mở/ẩn, ⌃⌥⌘1–4 mở thẳng một tab, ⌃⌥⌘M ghi chú mới. Tổ hợp ⌃⌥ cũ bị trùng với ứng dụng khác.",
        "Tab Mức dùng trong Công cụ đổi tên thành “Token”.",
      ],
    },
  },
  {
    version: "0.3.5",
    date: "2026-09-28",
    notes: {
      ko: [
        "창이 지금 보고 있는 데스크톱과 모니터에 열립니다. 다른 데스크톱으로 화면이 넘어가지 않습니다.",
        "Claude 데스크톱 앱이 기록한 5시간·주간 한도를 설정 없이 보여 줍니다.",
        "비밀번호 관리자가 “기록하지 말 것”으로 표시한 복사는 클립보드 기록에 남지 않습니다 (macOS).",
        "상태 탭에 GPU, 최근 그래프, CPU·메모리를 많이 쓰는 앱이 추가되었습니다.",
        "설정 → 시스템 모니터: 메뉴바에 수치 표시(macOS), CPU·메모리·온도·디스크·배터리 경고 알림.",
      ],
      en: [
        "The window opens on the desktop and display you are looking at, instead of switching you to another desktop.",
        "Shows the 5-hour and weekly limits the Claude desktop app records, with no setup.",
        "Copies a password manager marks as not to be recorded stay out of clipboard history (macOS).",
        "The Status tab adds GPU, recent graphs and the apps using the most CPU and memory.",
        "Settings → System monitor: a figure in the menu bar (macOS) and alerts for CPU, memory, temperature, disk and battery.",
      ],
      ja: [
        "ウインドウが今見ているデスクトップとディスプレイに開き、別のデスクトップへ切り替わりません。",
        "Claude デスクトップアプリが記録した 5 時間・週間の上限を設定なしで表示します。",
        "パスワード管理アプリが「記録しない」と印を付けたコピーはクリップボード履歴に残りません（macOS）。",
        "ステータスタブに GPU、直近のグラフ、CPU・メモリを多く使うアプリを追加。",
        "設定 → システムモニタ：メニューバーに数値表示（macOS）、CPU・メモリ・温度・ディスク・バッテリーの警告通知。",
      ],
      zh: [
        "窗口在你当前查看的桌面和显示器上打开，不再切换到其他桌面。",
        "无需设置即可显示 Claude 桌面应用记录的 5 小时与每周限额。",
        "密码管理器标记为“不记录”的复制内容不会进入剪贴板历史（macOS）。",
        "状态标签页新增 GPU、近期图表，以及 CPU 和内存占用最高的应用。",
        "设置 → 系统监控：菜单栏显示读数（macOS），以及 CPU、内存、温度、磁盘、电量警报。",
      ],
      es: [
        "La ventana se abre en el escritorio y la pantalla que estás mirando, sin llevarte a otro escritorio.",
        "Muestra sin configurar nada los límites de 5 h y semanales que registra la app de escritorio de Claude.",
        "Lo que un gestor de contraseñas marca para no registrar no entra en el historial del portapapeles (macOS).",
        "La pestaña Estado añade GPU, gráficas recientes y las apps que más CPU y memoria usan.",
        "Ajustes → Monitor del sistema: una cifra en la barra de menús (macOS) y alertas de CPU, memoria, temperatura, disco y batería.",
      ],
      fr: [
        "La fenêtre s'ouvre sur le bureau et l'écran que vous regardez, sans vous emmener sur un autre bureau.",
        "Affiche sans configuration les limites 5 h et hebdomadaires enregistrées par l'app de bureau Claude.",
        "Ce qu'un gestionnaire de mots de passe marque comme à ne pas enregistrer reste hors de l'historique (macOS).",
        "L'onglet État ajoute le GPU, des graphiques récents et les apps les plus gourmandes en CPU et mémoire.",
        "Réglages → Moniteur système : une valeur dans la barre des menus (macOS) et des alertes CPU, mémoire, température, disque et batterie.",
      ],
      de: [
        "Das Fenster öffnet auf dem Schreibtisch und Bildschirm, den Sie gerade ansehen, statt zu einem anderen Schreibtisch zu wechseln.",
        "Zeigt ohne Einrichtung die 5-Stunden- und Wochenlimits, die die Claude-Desktop-App aufzeichnet.",
        "Was ein Passwortmanager als nicht aufzuzeichnen markiert, landet nicht im Zwischenablage-Verlauf (macOS).",
        "Der Status-Tab zeigt jetzt GPU, aktuelle Diagramme und die Apps mit der höchsten CPU- und Speicherlast.",
        "Einstellungen → Systemmonitor: ein Wert in der Menüleiste (macOS) und Warnungen für CPU, Speicher, Temperatur, Speicherplatz und Akku.",
      ],
      vi: [
        "Cửa sổ mở trên màn hình nền và màn hình bạn đang xem, không chuyển bạn sang màn hình nền khác.",
        "Hiển thị giới hạn 5 giờ và hàng tuần mà ứng dụng Claude trên máy ghi lại, không cần cài đặt.",
        "Nội dung trình quản lý mật khẩu đánh dấu là không lưu sẽ không vào lịch sử clipboard (macOS).",
        "Tab Trạng thái thêm GPU, biểu đồ gần đây và các ứng dụng dùng nhiều CPU, bộ nhớ nhất.",
        "Cài đặt → Giám sát hệ thống: số liệu trên thanh menu (macOS) và cảnh báo CPU, bộ nhớ, nhiệt độ, ổ đĩa, pin.",
      ],
    },
  },
  {
    version: "0.3.4",
    date: "2026-09-28",
    notes: {
      ko: ["NVIDIA와 Z.AI를 별도 AI 제공자로 추가했습니다. 키는 제공자마다 따로 저장됩니다."],
      en: ["NVIDIA and Z.AI join as separate AI providers, each with its own stored key."],
      ja: ["NVIDIA と Z.AI を別々の AI プロバイダーとして追加。キーはプロバイダーごとに保存されます。"],
      zh: ["新增 NVIDIA 与 Z.AI 两个独立的 AI 服务商，各自单独保存密钥。"],
      es: ["NVIDIA y Z.AI se añaden como proveedores de IA separados, cada uno con su propia clave guardada."],
      fr: ["NVIDIA et Z.AI arrivent comme fournisseurs d'IA distincts, chacun avec sa propre clé enregistrée."],
      de: ["NVIDIA und Z.AI kommen als eigene KI-Anbieter hinzu, jeweils mit eigenem gespeichertem Schlüssel."],
      vi: ["Thêm NVIDIA và Z.AI làm hai nhà cung cấp AI riêng, mỗi bên lưu khóa riêng."],
    },
  },
  {
    version: "0.3.3",
    date: "2026-09-26",
    notes: {
      ko: ["모든 기능에 단축키가 생기고, 설정에 단축키 보기 페이지가 추가되었습니다.", "새 문어 앱 아이콘."],
      en: ["Every feature gets a shortcut, and Settings has a page listing them.", "A new octopus app icon."],
      ja: ["すべての機能にショートカットが付き、設定にショートカット一覧ページを追加。", "新しいタコのアプリアイコン。"],
      zh: ["所有功能都有了快捷键，设置中新增快捷键列表页面。", "全新的章鱼应用图标。"],
      es: ["Cada función tiene atajo y Ajustes incluye una página que los lista.", "Nuevo icono de pulpo."],
      fr: ["Chaque fonction a son raccourci, et les Réglages ont une page qui les liste.", "Nouvelle icône pieuvre."],
      de: ["Jede Funktion hat einen Kurzbefehl, und die Einstellungen listen sie auf einer eigenen Seite.", "Neues Oktopus-App-Symbol."],
      vi: ["Mọi tính năng đều có phím tắt, Cài đặt có trang liệt kê phím tắt.", "Biểu tượng bạch tuộc mới."],
    },
  },
  {
    version: "0.3.2",
    date: "2026-09-26",
    notes: {
      ko: ["클립보드 탭에서 메모를 직접 쓰고 고칠 수 있습니다. 메모는 복사한 항목과 한 목록에 보이고 자동으로 지워지지 않습니다."],
      en: ["Write and edit memos right in the Clipboard tab. They sit in the same list as copies and are never removed automatically."],
      ja: ["クリップボードタブでメモを直接書いて編集できます。コピーと同じ一覧に並び、自動では消えません。"],
      zh: ["可在剪贴板标签页直接撰写和编辑便签，便签与复制内容同列显示，不会被自动删除。"],
      es: ["Escribe y edita notas directamente en la pestaña Portapapeles; comparten lista con las copias y nunca se borran solas."],
      fr: ["Rédigez et modifiez des mémos dans l'onglet Presse-papiers ; ils côtoient les copies et ne sont jamais supprimés automatiquement."],
      de: ["Notizen direkt im Zwischenablage-Tab schreiben und bearbeiten; sie stehen neben den Kopien und werden nie automatisch gelöscht."],
      vi: ["Viết và sửa ghi chú ngay trong tab Clipboard; ghi chú nằm chung danh sách với nội dung sao chép và không tự bị xoá."],
    },
  },
  {
    version: "0.3.1",
    date: "2026-09-26",
    notes: {
      ko: [
        "메뉴바 아이콘: 왼쪽 클릭으로 열고, 오른쪽 클릭으로 종료합니다.",
        "설정 → 정보에 DeepL 계정 링크가 추가되었습니다.",
        "보안 강화: 콘텐츠 보안 정책 적용, 의존성 취약점 점검을 배포 조건으로, 정리 도구는 검증된 경로에서만 실행.",
      ],
      en: [
        "Menu bar icon: left click opens, right click quits.",
        "Settings → About links to your DeepL account.",
        "Hardening: a Content Security Policy, dependency advisories as a release gate, and the cleanup tool only runs from a verified path.",
      ],
      ja: [
        "メニューバーアイコン：左クリックで開き、右クリックで終了。",
        "設定 → 情報に DeepL アカウントへのリンクを追加。",
        "セキュリティ強化：コンテンツセキュリティポリシー、依存関係の脆弱性チェックをリリース条件に、クリーンアップは検証済みのパスからのみ実行。",
      ],
      zh: [
        "菜单栏图标：左键打开，右键退出。",
        "设置 → 关于 中新增 DeepL 账户链接。",
        "安全加固：启用内容安全策略，依赖漏洞检查成为发布条件，清理工具只从已验证路径运行。",
      ],
      es: [
        "Icono de la barra de menús: clic izquierdo abre, clic derecho cierra la app.",
        "Ajustes → Acerca de enlaza a tu cuenta de DeepL.",
        "Seguridad: política de seguridad de contenido, avisos de dependencias como requisito de publicación y la limpieza solo se ejecuta desde una ruta verificada.",
      ],
      fr: [
        "Icône de la barre des menus : clic gauche pour ouvrir, clic droit pour quitter.",
        "Réglages → À propos renvoie vers votre compte DeepL.",
        "Renforcement : politique de sécurité du contenu, alertes de dépendances bloquantes pour la publication, nettoyage lancé uniquement depuis un chemin vérifié.",
      ],
      de: [
        "Menüleistensymbol: Linksklick öffnet, Rechtsklick beendet.",
        "Einstellungen → Über verlinkt Ihr DeepL-Konto.",
        "Härtung: Content-Security-Policy, Abhängigkeitswarnungen als Release-Bedingung, Aufräumen nur aus einem geprüften Pfad.",
      ],
      vi: [
        "Biểu tượng thanh menu: nhấp trái để mở, nhấp phải để thoát.",
        "Cài đặt → Thông tin có liên kết tới tài khoản DeepL.",
        "Tăng cường bảo mật: chính sách bảo mật nội dung, kiểm tra lỗ hổng thư viện là điều kiện phát hành, công cụ dọn dẹp chỉ chạy từ đường dẫn đã xác minh.",
      ],
    },
  },
  {
    version: "0.3.0",
    date: "2026-09-24",
    notes: {
      ko: ["정리 패널이 불러오는 중이면 그렇다고 정확히 표시합니다."],
      en: ["The cleanup panel says it is loading while it loads."],
      ja: ["クリーンアップパネルが読み込み中であることを正しく表示します。"],
      zh: ["清理面板加载时会如实显示正在加载。"],
      es: ["El panel de limpieza indica que está cargando mientras carga."],
      fr: ["Le panneau de nettoyage indique qu'il charge pendant le chargement."],
      de: ["Das Aufräumen-Panel zeigt beim Laden an, dass es lädt."],
      vi: ["Bảng dọn dẹp hiển thị đúng là đang tải khi đang tải."],
    },
  },
  {
    version: "0.2.30",
    date: "2026-09-24",
    notes: {
      ko: ["서명된 업데이트를 앱 안에서 바로 설치합니다.", "로그인하지 않은 상태에서도 새 버전을 알려 줍니다.", "창이 더 빨리 열립니다."],
      en: ["Signed updates install in place from inside the app.", "New releases are announced even when signed out.", "The window opens faster."],
      ja: ["署名済みアップデートをアプリ内でそのままインストール。", "ログインしていなくても新バージョンを通知。", "ウインドウがより速く開きます。"],
      zh: ["已签名的更新可在应用内直接安装。", "未登录时也会提示新版本。", "窗口打开更快。"],
      es: ["Las actualizaciones firmadas se instalan desde la propia app.", "Se avisa de nuevas versiones aunque no hayas iniciado sesión.", "La ventana se abre más rápido."],
      fr: ["Les mises à jour signées s'installent depuis l'app.", "Les nouvelles versions sont signalées même sans connexion.", "La fenêtre s'ouvre plus vite."],
      de: ["Signierte Updates werden direkt in der App installiert.", "Neue Versionen werden auch ohne Anmeldung angezeigt.", "Das Fenster öffnet schneller."],
      vi: ["Cập nhật đã ký được cài ngay trong ứng dụng.", "Vẫn báo có phiên bản mới khi chưa đăng nhập.", "Cửa sổ mở nhanh hơn."],
    },
  },
  {
    version: "0.2.29",
    date: "2026-09-23",
    notes: {
      ko: ["새 버전이 나오면 설정 톱니에 점으로 알려 줍니다."],
      en: ["A dot on the settings gear tells you when a newer release exists."],
      ja: ["新しいバージョンがあると設定の歯車にドットで知らせます。"],
      zh: ["有新版本时，设置齿轮上会出现圆点提示。"],
      es: ["Un punto en el engranaje de ajustes avisa cuando hay una versión nueva."],
      fr: ["Un point sur l'engrenage des réglages signale une nouvelle version."],
      de: ["Ein Punkt am Einstellungszahnrad zeigt eine neuere Version an."],
      vi: ["Dấu chấm trên bánh răng cài đặt báo có phiên bản mới."],
    },
  },
  {
    version: "0.2.28",
    date: "2026-09-23",
    notes: {
      ko: ["도구에 시스템 상태와 앱 정리(Mole)가 추가되었습니다.", "창 고정, 문장 다듬기 작업 공간이 생겼습니다."],
      en: ["Tools adds system status and in-app cleanup (Mole).", "Pin the window, and a rewrite workspace for polishing text."],
      ja: ["ツールにシステムステータスとアプリ内クリーンアップ（Mole）を追加。", "ウインドウの固定と、文章を整える作業スペース。"],
      zh: ["工具新增系统状态与应用内清理（Mole）。", "可固定窗口，并新增润色文字的改写工作区。"],
      es: ["Herramientas añade estado del sistema y limpieza integrada (Mole).", "Fijar la ventana y un espacio para reescribir textos."],
      fr: ["Les Outils ajoutent l'état du système et le nettoyage intégré (Mole).", "Épingler la fenêtre, et un espace de réécriture."],
      de: ["Werkzeuge erhält Systemstatus und Aufräumen in der App (Mole).", "Fenster anheften und ein Arbeitsbereich zum Umformulieren."],
      vi: ["Công cụ thêm trạng thái hệ thống và dọn dẹp trong ứng dụng (Mole).", "Ghim cửa sổ và không gian viết lại văn bản."],
    },
  },
  {
    version: "0.2.27",
    date: "2026-09-21",
    notes: {
      ko: ["Claude·Codex·Gemini·Grok·Cursor 계정으로 로그인해 쓸 수 있습니다.", "챗에 이미지를 붙일 수 있습니다."],
      en: ["Sign in with Claude, Codex, Gemini, Grok or Cursor.", "Attach images in chat."],
      ja: ["Claude・Codex・Gemini・Grok・Cursor のアカウントでログインして使えます。", "チャットに画像を添付できます。"],
      zh: ["可使用 Claude、Codex、Gemini、Grok、Cursor 账户登录使用。", "聊天中可附加图片。"],
      es: ["Inicia sesión con Claude, Codex, Gemini, Grok o Cursor.", "Adjunta imágenes en el chat."],
      fr: ["Connexion avec Claude, Codex, Gemini, Grok ou Cursor.", "Joindre des images dans le chat."],
      de: ["Anmeldung mit Claude, Codex, Gemini, Grok oder Cursor.", "Bilder im Chat anhängen."],
      vi: ["Đăng nhập bằng Claude, Codex, Gemini, Grok hoặc Cursor.", "Đính kèm ảnh trong chat."],
    },
  },
]

export const LATEST_ENTRY = CHANGELOG[0]
