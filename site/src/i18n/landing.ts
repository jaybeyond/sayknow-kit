// Landing and feature-overview copy, plus the example content drawn inside
// the app mockups. Labels the app itself shows (tab names, chips, filters)
// are not here: the mockups take those from the app's own strings.
import type { FeatureId, Locale } from "../data/site"

export type LandingSection = { headline: string; body: string; bullets: string[] }

export type LandingCopy = {
  eyebrow: string
  headline: string
  body: string
  sections: Record<FeatureId, LandingSection>
  stats: { translate: string; providers: string; usage: string }
  factsTitle: string
  closingHeadline: string
  closingBody: string
  mock: {
    from: string
    to: string
    input: string
    output: string
    chatQuestion: string
    chatAnswer: string
    chatFollowUp: string
    memo: string
    ago2: string
    ago5: string
    resetFive: string
    resetWeek: string
    signedIn: string
    models: string
  }
}

const EN_OUTPUT = "Next Tuesday's meeting has been moved to 3 PM. Please share the materials by Monday."

export const LANDING: Record<Locale, LandingCopy> = {
  ko: {
    eyebrow: "메뉴바 AI 키트",
    headline: "번역, 챗, 클립보드.\n단축키 하나면 전부.",
    body: "SayKnow Kit은 메뉴바에 사는 작은 도구 상자입니다. 단축키를 누르면 지금 보고 있는 화면 위에 바로 열립니다.",
    sections: {
      translate: {
        headline: "쓰는 동안\n번역이 끝나 있습니다.",
        body: "입력을 멈추고 1.5초면 번역됩니다. 마음에 안 들면 버튼 하나로 정중하게, 짧게, 비즈니스 말투로.",
        bullets: ["37개 언어, 원문 언어 자동 감지", "용어집으로 회사명·제품명을 늘 같게", "DeepL 엔진도 연결 가능"],
      },
      chat: {
        headline: "궁금하면\n그 자리에서 묻습니다.",
        body: "번역 창을 닫지 않고 옆 탭에서 바로 질문합니다. 대화는 제목이 붙어 쌓이고, 이미지도 붙일 수 있습니다.",
        bullets: ["답변 다시 만들기·고치기·멈추기", "로그인한 계정으로 이미지 첨부", "대화는 이 기기에만 저장"],
      },
      clipboard: {
        headline: "복사한 건\n잃어버리지 않습니다.",
        body: "복사한 글이 알아서 쌓이고, 검색하면 바로 나옵니다. 메모도 같은 목록에 직접 씁니다.",
        bullets: ["고정한 항목은 지워지지 않음", "비밀번호 관리자 복사는 기록 안 함", "항목 하나를 바로 번역으로"],
      },
      system: {
        headline: "내 Mac이 지금\n뭘 하는지 보입니다.",
        body: "CPU, GPU, 메모리, 온도를 작은 그래프로. 무거운 앱을 찾아 주고, 과열이나 디스크 부족은 알림으로 알려 줍니다.",
        bullets: ["메뉴바에 CPU·온도 표시", "외부 모니터 밝기를 슬라이더 하나로", "캐시 정리까지 앱 안에서"],
      },
      usage: {
        headline: "Claude Code, Codex.\n얼마나 남았는지 한눈에.",
        body: "토큰은 기기에 남은 기록에서 읽고 Codex 한도는 기존 로그인으로 OpenAI에서 갱신합니다. 오래된 값은 현재 한도로 표시하지 않습니다.",
        bullets: ["5시간 블록 남은 시간과 소모 속도", "모델별 토큰", "Claude 앱 한도까지 자동으로"],
      },
      providers: {
        headline: "쓰던 AI를\n그대로 연결합니다.",
        body: "OpenRouter 키 하나로 360개가 넘는 모델, 또는 Claude·Codex·Gemini·Grok·Cursor 계정으로 로그인. 요금은 각자 계정으로.",
        bullets: ["NVIDIA, Z.AI, 직접 입력한 주소", "키는 키체인에만 저장", "실패하면 대체 모델로 다시 시도"],
      },
    },
    stats: { translate: "입력을 멈춘 뒤 번역까지", providers: "OpenRouter 키 하나로 쓰는 모델", usage: "5시간·주간 한도와 리셋 시각" },
    factsTitle: "알아두면 좋은 사실",
    closingHeadline: "지금 메뉴바에 올려 두세요.",
    closingBody: "무료, 오픈소스. 가입도 결제도 없습니다.",
    mock: {
      from: "한국어",
      to: "English",
      input: "다음 주 화요일 회의는 오후 3시로 옮겨졌습니다. 자료는 월요일까지 공유 부탁드려요.",
      output: EN_OUTPUT,
      chatQuestion: "“circle back”은 회의에서 어떤 뉘앙스야?",
      chatAnswer: "“나중에 다시 이야기하자”는 뜻이에요. 결론을 미루자는 부드러운 표현이라 거절처럼 들리지 않아요.",
      chatFollowUp: "좀 더 격식 있게 말하려면?",
      memo: "금요일까지 견적서 v2 보내기",
      ago2: "2분 전",
      ago5: "5분 전",
      resetFive: "리셋까지 약 2시간 40분",
      resetWeek: "리셋까지 약 3일",
      signedIn: "로그인됨",
      models: "모델 360개 이상",
    },
  },
  en: {
    eyebrow: "An AI kit for the menu bar",
    headline: "Translate, chat, clipboard.\nOne shortcut away.",
    body: "SayKnow Kit is a small toolbox that lives in your menu bar. Press the shortcut and it opens right over whatever you are looking at.",
    sections: {
      translate: {
        headline: "The translation is done\nbefore you are.",
        body: "Stop typing and 1.5 seconds later it is translated. Not quite right? One button makes it polite, shorter or business-like.",
        bullets: ["37 languages, source detected for you", "A glossary keeps names consistent", "Runs on DeepL too"],
      },
      chat: {
        headline: "Wondering?\nAsk right there.",
        body: "Ask in the next tab without closing the translator. Conversations get titles and stack up, and you can attach images.",
        bullets: ["Regenerate, edit or stop an answer", "Images with your signed-in account", "Conversations stay on this device"],
      },
      clipboard: {
        headline: "Nothing you copy\ngets lost.",
        body: "What you copy piles up on its own and comes back with a search. Write memos in the same list.",
        bullets: ["Pinned items are never cleared", "Password-manager copies are never kept", "Send any item straight to Translate"],
      },
      system: {
        headline: "See what your Mac\nis doing right now.",
        body: "CPU, GPU, memory and temperature as small graphs. It finds the heavy apps and tells you when things run hot or the disk fills up.",
        bullets: ["CPU or temperature in the menu bar", "External monitor brightness on one slider", "Clean caches from inside the app"],
      },
      usage: {
        headline: "Claude Code, Codex.\nWhat is left, at a glance.",
        body: "Tokens come from logs already on your device. Codex limits refresh from OpenAI using the existing login; old readings are never shown as current.",
        bullets: ["Time left and burn rate in the 5-hour block", "Tokens per model", "Claude app limits, automatically"],
      },
      providers: {
        headline: "Bring the AI\nyou already use.",
        body: "360+ models on one OpenRouter key, or sign in with Claude, Codex, Gemini, Grok or Cursor. Billed to your own account.",
        bullets: ["NVIDIA, Z.AI and custom endpoints", "Keys live only in the Keychain", "Falls back to another model on failure"],
      },
    },
    stats: { translate: "from your last keystroke to a translation", providers: "models on one OpenRouter key", usage: "5-hour and weekly limits with reset times" },
    factsTitle: "Good to know",
    closingHeadline: "Put it in your menu bar today.",
    closingBody: "Free and open source. No sign-up, no payment.",
    mock: {
      from: "English",
      to: "한국어",
      input: "Next Tuesday's meeting has moved to 3 PM. Please share the materials by Monday.",
      output: "다음 주 화요일 회의가 오후 3시로 옮겨졌습니다. 자료는 월요일까지 공유 부탁드립니다.",
      chatQuestion: "How does “circle back” sound in a meeting?",
      chatAnswer: "It means “let's revisit this later” — a soft way to postpone a decision without it sounding like a no.",
      chatFollowUp: "And a more formal way to say it?",
      memo: "Send quote v2 by Friday",
      ago2: "2 min ago",
      ago5: "5 min ago",
      resetFive: "resets in about 2 h 40 min",
      resetWeek: "resets in about 3 days",
      signedIn: "Signed in",
      models: "360+ models",
    },
  },
  ja: {
    eyebrow: "メニューバーの AI キット",
    headline: "翻訳、チャット、クリップボード。\nショートカット一つで。",
    body: "SayKnow Kit はメニューバーに住む小さな道具箱です。ショートカットを押すと、いま見ている画面の上にすぐ開きます。",
    sections: {
      translate: {
        headline: "書き終わる頃には\n翻訳が終わっています。",
        body: "入力を止めて 1.5 秒で翻訳。気に入らなければボタン一つで丁寧に、短く、ビジネス調に。",
        bullets: ["37 言語、原文の言語は自動検出", "用語集で社名・製品名をいつも同じに", "DeepL エンジンにも対応"],
      },
      chat: {
        headline: "気になったら\nその場で聞けます。",
        body: "翻訳ウインドウを閉じずに隣のタブで質問。会話にはタイトルが付いて残り、画像も添付できます。",
        bullets: ["回答の再生成・編集・停止", "ログインしたアカウントで画像添付", "会話はこの端末にのみ保存"],
      },
      clipboard: {
        headline: "コピーしたものは\nなくしません。",
        body: "コピーした文章が自動でたまり、検索すればすぐ出てきます。メモも同じ一覧に直接書けます。",
        bullets: ["ピン留めした項目は消えない", "パスワード管理アプリのコピーは記録しない", "項目をそのまま翻訳へ"],
      },
      system: {
        headline: "Mac の状態が\nひと目でわかります。",
        body: "CPU、GPU、メモリ、温度を小さなグラフで。重いアプリを見つけ、高温やディスク不足は通知で知らせます。",
        bullets: ["メニューバーに CPU・温度を表示", "外部モニターの明るさをスライダー一つで", "キャッシュ整理もアプリの中で"],
      },
      usage: {
        headline: "Claude Code、Codex。\n残りがひと目で。",
        body: "トークンは端末の記録から取得し、Codex の上限は既存のログインで OpenAI から更新します。古い値を現在値として表示しません。",
        bullets: ["5 時間ブロックの残り時間と消費ペース", "モデル別トークン", "Claude アプリの上限も自動で"],
      },
      providers: {
        headline: "使っている AI を\nそのままつなぎます。",
        body: "OpenRouter のキー一つで 360 以上のモデル、または Claude・Codex・Gemini・Grok・Cursor でログイン。料金はそれぞれのアカウントへ。",
        bullets: ["NVIDIA、Z.AI、任意のエンドポイント", "キーはキーチェーンにだけ保存", "失敗したら代替モデルで再試行"],
      },
    },
    stats: { translate: "入力を止めてから翻訳まで", providers: "OpenRouter のキー一つで使えるモデル", usage: "5 時間・週間の上限とリセット時刻" },
    factsTitle: "知っておきたいこと",
    closingHeadline: "今日からメニューバーに。",
    closingBody: "無料、オープンソース。登録も支払いもありません。",
    mock: {
      from: "日本語",
      to: "English",
      input: "来週火曜の会議は午後3時に変更になりました。資料は月曜までに共有をお願いします。",
      output: EN_OUTPUT,
      chatQuestion: "会議での「circle back」はどんなニュアンス？",
      chatAnswer: "「後でまた話しましょう」という意味です。結論を先送りする柔らかい言い方で、断りには聞こえません。",
      chatFollowUp: "もっと丁寧に言うなら？",
      memo: "金曜までに見積書 v2 を送る",
      ago2: "2 分前",
      ago5: "5 分前",
      resetFive: "リセットまで約 2 時間 40 分",
      resetWeek: "リセットまで約 3 日",
      signedIn: "ログイン済み",
      models: "360 以上のモデル",
    },
  },
  zh: {
    eyebrow: "菜单栏里的 AI 工具包",
    headline: "翻译、聊天、剪贴板。\n一个快捷键全搞定。",
    body: "SayKnow Kit 是住在菜单栏里的小工具箱。按下快捷键，它就在你正在看的屏幕上打开。",
    sections: {
      translate: {
        headline: "你还在写，\n翻译已经好了。",
        body: "停止输入 1.5 秒即完成翻译。不满意？一键改得更礼貌、更简短或更商务。",
        bullets: ["37 种语言，自动识别原文", "术语表让公司名、产品名始终一致", "也可接入 DeepL"],
      },
      chat: {
        headline: "有疑问，\n当场就问。",
        body: "不用关闭翻译窗口，在旁边的标签页直接提问。对话自动命名并保存，还可以附加图片。",
        bullets: ["重新生成、编辑、中途停止", "登录账户即可附加图片", "对话只保存在本设备"],
      },
      clipboard: {
        headline: "复制过的内容\n不会丢。",
        body: "复制的文字自动累积，一搜就能找到。便签也直接写在同一个列表里。",
        bullets: ["置顶项不会被清除", "密码管理器的复制内容不记录", "一键发送到翻译"],
      },
      system: {
        headline: "你的 Mac 在忙什么，\n一目了然。",
        body: "用小图表显示 CPU、GPU、内存和温度。帮你找出占用高的应用，过热或磁盘不足时发送通知。",
        bullets: ["菜单栏显示 CPU 或温度", "一个滑块调节外接显示器亮度", "在应用内清理缓存"],
      },
      usage: {
        headline: "Claude Code、Codex，\n还剩多少一眼看清。",
        body: "Token 数从本机记录读取。Codex 额度使用现有登录信息从 OpenAI 更新；旧记录不会显示为当前值。",
        bullets: ["5 小时区块剩余时间与消耗速度", "按模型统计 token", "自动读取 Claude 应用额度"],
      },
      providers: {
        headline: "直接连接\n你正在用的 AI。",
        body: "一个 OpenRouter 密钥用 360 多个模型，或用 Claude、Codex、Gemini、Grok、Cursor 登录。费用记在你自己的账户。",
        bullets: ["NVIDIA、Z.AI 与自定义地址", "密钥只存放在钥匙串", "失败时自动换备用模型重试"],
      },
    },
    stats: { translate: "从停止输入到完成翻译", providers: "一个 OpenRouter 密钥可用的模型", usage: "5 小时与每周额度及重置时间" },
    factsTitle: "值得了解",
    closingHeadline: "现在就放进菜单栏。",
    closingBody: "免费开源。无需注册，无需付费。",
    mock: {
      from: "中文",
      to: "English",
      input: "下周二的会议改到下午3点了，资料请在周一前分享。",
      output: EN_OUTPUT,
      chatQuestion: "会议里说“circle back”是什么语气？",
      chatAnswer: "意思是“之后再讨论”。这是推迟结论的委婉说法，听起来不像拒绝。",
      chatFollowUp: "更正式的说法呢？",
      memo: "周五前发送报价单 v2",
      ago2: "2 分钟前",
      ago5: "5 分钟前",
      resetFive: "约 2 小时 40 分后重置",
      resetWeek: "约 3 天后重置",
      signedIn: "已登录",
      models: "360 多个模型",
    },
  },
  es: {
    eyebrow: "Un kit de IA en la barra de menús",
    headline: "Traducir, chatear, portapapeles.\nA un atajo.",
    body: "SayKnow Kit es una pequeña caja de herramientas en tu barra de menús. Pulsa el atajo y se abre sobre lo que estés mirando.",
    sections: {
      translate: {
        headline: "La traducción termina\nantes que tú.",
        body: "Deja de escribir y en 1,5 segundos está traducido. ¿No te convence? Un botón lo hace más formal, más corto o más profesional.",
        bullets: ["37 idiomas, origen detectado solo", "Un glosario mantiene los nombres iguales", "También funciona con DeepL"],
      },
      chat: {
        headline: "¿Dudas?\nPregunta ahí mismo.",
        body: "Pregunta en la pestaña de al lado sin cerrar el traductor. Las conversaciones se titulan solas y puedes adjuntar imágenes.",
        bullets: ["Regenera, edita o detén una respuesta", "Imágenes con tu cuenta conectada", "Las conversaciones se quedan en tu equipo"],
      },
      clipboard: {
        headline: "Nada de lo que copias\nse pierde.",
        body: "Lo que copias se acumula solo y vuelve con una búsqueda. Escribe notas en la misma lista.",
        bullets: ["Lo fijado nunca se borra", "No guarda copias de gestores de contraseñas", "Envía cualquier elemento a Traducir"],
      },
      system: {
        headline: "Mira qué está haciendo\ntu Mac ahora mismo.",
        body: "CPU, GPU, memoria y temperatura en pequeñas gráficas. Encuentra las apps pesadas y te avisa si se calienta o se llena el disco.",
        bullets: ["CPU o temperatura en la barra de menús", "Brillo de monitores externos con un control", "Limpia cachés desde la app"],
      },
      usage: {
        headline: "Claude Code, Codex.\nLo que te queda, de un vistazo.",
        body: "Los tokens se leen de los registros locales. Los límites de Codex se actualizan desde OpenAI con la sesión existente; las lecturas antiguas no se muestran como actuales.",
        bullets: ["Tiempo restante y ritmo del bloque de 5 horas", "Tokens por modelo", "Límites de la app de Claude, solos"],
      },
      providers: {
        headline: "Trae la IA\nque ya usas.",
        body: "Más de 360 modelos con una clave de OpenRouter, o inicia sesión con Claude, Codex, Gemini, Grok o Cursor. Se cobra en tu propia cuenta.",
        bullets: ["NVIDIA, Z.AI y endpoints propios", "Las claves solo en el Llavero", "Si falla, prueba con otro modelo"],
      },
    },
    stats: { translate: "desde la última tecla hasta la traducción", providers: "modelos con una clave de OpenRouter", usage: "límites de 5 horas y semanales con reinicio" },
    factsTitle: "Conviene saber",
    closingHeadline: "Ponlo hoy en tu barra de menús.",
    closingBody: "Gratis y de código abierto. Sin registro ni pagos.",
    mock: {
      from: "Español",
      to: "English",
      input: "La reunión del próximo martes pasa a las 3 p. m. Comparte los materiales antes del lunes, por favor.",
      output: EN_OUTPUT,
      chatQuestion: "¿Qué matiz tiene “circle back” en una reunión?",
      chatAnswer: "Significa “lo retomamos más tarde”: una forma suave de aplazar una decisión sin que suene a un no.",
      chatFollowUp: "¿Y una forma más formal?",
      memo: "Enviar presupuesto v2 antes del viernes",
      ago2: "hace 2 min",
      ago5: "hace 5 min",
      resetFive: "se reinicia en unas 2 h 40 min",
      resetWeek: "se reinicia en unos 3 días",
      signedIn: "Sesión iniciada",
      models: "más de 360 modelos",
    },
  },
  fr: {
    eyebrow: "Un kit d'IA dans la barre des menus",
    headline: "Traduire, discuter, presse-papiers.\nÀ un raccourci.",
    body: "SayKnow Kit est une petite boîte à outils qui vit dans la barre des menus. Un raccourci, et elle s'ouvre au-dessus de ce que vous regardez.",
    sections: {
      translate: {
        headline: "La traduction est prête\navant vous.",
        body: "Arrêtez de taper : 1,5 seconde plus tard, c'est traduit. Pas tout à fait ça ? Un bouton rend le texte poli, plus court ou professionnel.",
        bullets: ["37 langues, langue source détectée", "Un glossaire garde les noms constants", "Fonctionne aussi avec DeepL"],
      },
      chat: {
        headline: "Une question ?\nPosez-la sur place.",
        body: "Demandez dans l'onglet voisin sans fermer le traducteur. Les conversations reçoivent un titre et vous pouvez joindre des images.",
        bullets: ["Régénérer, modifier ou arrêter une réponse", "Images avec votre compte connecté", "Les conversations restent sur l'appareil"],
      },
      clipboard: {
        headline: "Rien de ce que vous copiez\nne se perd.",
        body: "Ce que vous copiez s'accumule tout seul et revient d'une recherche. Écrivez vos mémos dans la même liste.",
        bullets: ["Les épingles ne sont jamais effacées", "Aucune copie de gestionnaire de mots de passe", "Envoyez un élément vers Traduire"],
      },
      system: {
        headline: "Voyez ce que fait\nvotre Mac, maintenant.",
        body: "CPU, GPU, mémoire et température en petits graphiques. Il repère les apps gourmandes et prévient en cas de surchauffe ou de disque plein.",
        bullets: ["CPU ou température dans la barre des menus", "Luminosité des écrans externes d'un curseur", "Nettoyage des caches dans l'app"],
      },
      usage: {
        headline: "Claude Code, Codex.\nCe qu'il reste, d'un coup d'œil.",
        body: "Les jetons proviennent des journaux locaux. Les quotas Codex sont actualisés auprès d'OpenAI avec la session existante ; les anciens relevés ne sont pas affichés comme actuels.",
        bullets: ["Temps restant et rythme du bloc de 5 heures", "Jetons par modèle", "Limites de l'app Claude, automatiquement"],
      },
      providers: {
        headline: "Gardez l'IA\nque vous utilisez déjà.",
        body: "Plus de 360 modèles avec une clé OpenRouter, ou connexion avec Claude, Codex, Gemini, Grok ou Cursor. Facturé sur votre propre compte.",
        bullets: ["NVIDIA, Z.AI et adresses personnalisées", "Les clés uniquement dans le Trousseau", "Un autre modèle prend le relais en cas d'échec"],
      },
    },
    stats: { translate: "de la dernière touche à la traduction", providers: "modèles avec une clé OpenRouter", usage: "limites 5 h et hebdomadaires avec réinitialisation" },
    factsTitle: "À savoir",
    closingHeadline: "Installez-le dans votre barre des menus.",
    closingBody: "Gratuit et open source. Ni inscription, ni paiement.",
    mock: {
      from: "Français",
      to: "English",
      input: "La réunion de mardi prochain est déplacée à 15 h. Merci de partager les documents d'ici lundi.",
      output: EN_OUTPUT,
      chatQuestion: "Quelle nuance a « circle back » en réunion ?",
      chatAnswer: "Ça veut dire « on en reparle plus tard » : une façon douce de reporter une décision sans que ça sonne comme un refus.",
      chatFollowUp: "Et une tournure plus formelle ?",
      memo: "Envoyer le devis v2 avant vendredi",
      ago2: "il y a 2 min",
      ago5: "il y a 5 min",
      resetFive: "réinitialisation dans environ 2 h 40",
      resetWeek: "réinitialisation dans environ 3 jours",
      signedIn: "Connecté",
      models: "plus de 360 modèles",
    },
  },
  de: {
    eyebrow: "Ein KI-Kit für die Menüleiste",
    headline: "Übersetzen, Chat, Zwischenablage.\nEin Kurzbefehl entfernt.",
    body: "SayKnow Kit ist ein kleiner Werkzeugkasten in der Menüleiste. Kurzbefehl drücken, und er öffnet sich über allem, was Sie gerade ansehen.",
    sections: {
      translate: {
        headline: "Die Übersetzung ist fertig,\nbevor Sie es sind.",
        body: "Tippen stoppen – 1,5 Sekunden später ist es übersetzt. Nicht ganz passend? Ein Knopf macht es höflicher, kürzer oder geschäftlicher.",
        bullets: ["37 Sprachen, Ausgangssprache erkannt", "Ein Glossar hält Namen einheitlich", "Läuft auch mit DeepL"],
      },
      chat: {
        headline: "Eine Frage?\nGleich dort stellen.",
        body: "Im Nachbar-Tab fragen, ohne den Übersetzer zu schließen. Unterhaltungen bekommen Titel, Bilder lassen sich anhängen.",
        bullets: ["Antwort neu erzeugen, bearbeiten, stoppen", "Bilder mit Ihrem angemeldeten Konto", "Unterhaltungen bleiben auf dem Gerät"],
      },
      clipboard: {
        headline: "Nichts, was Sie kopieren,\ngeht verloren.",
        body: "Was Sie kopieren, sammelt sich von selbst und ist per Suche sofort da. Notizen schreiben Sie in dieselbe Liste.",
        bullets: ["Angeheftetes wird nie gelöscht", "Keine Kopien aus Passwortmanagern", "Einträge direkt an Übersetzen senden"],
      },
      system: {
        headline: "Sehen, was Ihr Mac\ngerade tut.",
        body: "CPU, GPU, Speicher und Temperatur als kleine Diagramme. Findet die schweren Apps und warnt bei Hitze oder vollem Laufwerk.",
        bullets: ["CPU oder Temperatur in der Menüleiste", "Monitorhelligkeit mit einem Regler", "Caches in der App aufräumen"],
      },
      usage: {
        headline: "Claude Code, Codex.\nWas übrig ist, auf einen Blick.",
        body: "Tokens stammen aus lokalen Protokollen. Codex-Limits werden mit der bestehenden Anmeldung bei OpenAI aktualisiert; alte Messwerte erscheinen nicht als aktuell.",
        bullets: ["Restzeit und Verbrauch im 5-Stunden-Block", "Tokens pro Modell", "Limits der Claude-App automatisch"],
      },
      providers: {
        headline: "Die KI mitbringen,\ndie Sie schon nutzen.",
        body: "Über 360 Modelle mit einem OpenRouter-Schlüssel oder Anmeldung mit Claude, Codex, Gemini, Grok oder Cursor. Abgerechnet über Ihr eigenes Konto.",
        bullets: ["NVIDIA, Z.AI und eigene Adressen", "Schlüssel nur im Schlüsselbund", "Bei Fehlern ein Ersatzmodell"],
      },
    },
    stats: { translate: "vom letzten Tastendruck bis zur Übersetzung", providers: "Modelle mit einem OpenRouter-Schlüssel", usage: "5-Stunden- und Wochenlimits mit Reset-Zeit" },
    factsTitle: "Gut zu wissen",
    closingHeadline: "Holen Sie es sich in die Menüleiste.",
    closingBody: "Kostenlos und Open Source. Keine Anmeldung, keine Zahlung.",
    mock: {
      from: "Deutsch",
      to: "English",
      input: "Das Meeting am nächsten Dienstag ist auf 15 Uhr verschoben. Bitte teilt die Unterlagen bis Montag.",
      output: EN_OUTPUT,
      chatQuestion: "Wie klingt „circle back“ in einem Meeting?",
      chatAnswer: "Es heißt „wir kommen später darauf zurück“ – eine sanfte Art, eine Entscheidung zu vertagen, ohne nach Nein zu klingen.",
      chatFollowUp: "Und etwas förmlicher?",
      memo: "Angebot v2 bis Freitag senden",
      ago2: "vor 2 Min.",
      ago5: "vor 5 Min.",
      resetFive: "Reset in etwa 2 Std. 40 Min.",
      resetWeek: "Reset in etwa 3 Tagen",
      signedIn: "Angemeldet",
      models: "über 360 Modelle",
    },
  },
  vi: {
    eyebrow: "Bộ công cụ AI trên thanh menu",
    headline: "Dịch, chat, clipboard.\nChỉ một phím tắt.",
    body: "SayKnow Kit là hộp công cụ nhỏ nằm trên thanh menu. Nhấn phím tắt là nó mở ngay trên màn hình bạn đang xem.",
    sections: {
      translate: {
        headline: "Bạn vừa gõ xong,\nbản dịch đã có.",
        body: "Ngừng gõ 1,5 giây là có bản dịch. Chưa ưng? Một nút là thành lịch sự, ngắn gọn hay trang trọng.",
        bullets: ["37 ngôn ngữ, tự nhận diện ngôn ngữ gốc", "Bảng thuật ngữ giữ tên riêng nhất quán", "Dùng được cả DeepL"],
      },
      chat: {
        headline: "Thắc mắc?\nHỏi ngay tại chỗ.",
        body: "Hỏi ở tab bên cạnh mà không cần đóng trình dịch. Cuộc trò chuyện tự có tiêu đề và có thể đính kèm ảnh.",
        bullets: ["Tạo lại, sửa hoặc dừng câu trả lời", "Đính kèm ảnh khi đã đăng nhập", "Trò chuyện chỉ lưu trên máy này"],
      },
      clipboard: {
        headline: "Những gì bạn sao chép\nkhông bị mất.",
        body: "Nội dung sao chép tự tích lại, tìm là thấy ngay. Ghi chú cũng viết thẳng vào cùng danh sách.",
        bullets: ["Mục đã ghim không bao giờ bị xoá", "Không lưu nội dung từ trình quản lý mật khẩu", "Gửi một mục thẳng sang Dịch"],
      },
      system: {
        headline: "Máy Mac đang làm gì,\nnhìn là thấy.",
        body: "CPU, GPU, bộ nhớ và nhiệt độ dưới dạng biểu đồ nhỏ. Tìm ra ứng dụng nặng và báo khi máy nóng hay ổ đĩa sắp đầy.",
        bullets: ["Hiện CPU hoặc nhiệt độ trên thanh menu", "Chỉnh sáng màn hình ngoài bằng một thanh trượt", "Dọn bộ nhớ đệm ngay trong ứng dụng"],
      },
      usage: {
        headline: "Claude Code, Codex.\nCòn lại bao nhiêu, nhìn là biết.",
        body: "Token lấy từ nhật ký trên máy. Hạn mức Codex được cập nhật từ OpenAI bằng phiên đăng nhập hiện có; số liệu cũ không được hiển thị là hiện tại.",
        bullets: ["Thời gian còn lại và tốc độ dùng của khối 5 giờ", "Token theo mô hình", "Tự đọc hạn mức của ứng dụng Claude"],
      },
      providers: {
        headline: "Dùng luôn AI\nbạn đang có.",
        body: "Hơn 360 mô hình với một khóa OpenRouter, hoặc đăng nhập bằng Claude, Codex, Gemini, Grok, Cursor. Tính phí vào tài khoản của bạn.",
        bullets: ["NVIDIA, Z.AI và địa chỉ tùy chỉnh", "Khóa chỉ nằm trong Keychain", "Lỗi thì tự thử mô hình dự phòng"],
      },
    },
    stats: { translate: "từ lần gõ cuối đến bản dịch", providers: "mô hình với một khóa OpenRouter", usage: "hạn mức 5 giờ, hàng tuần và giờ reset" },
    factsTitle: "Nên biết",
    closingHeadline: "Đặt nó lên thanh menu ngay hôm nay.",
    closingBody: "Miễn phí, mã nguồn mở. Không đăng ký, không trả phí.",
    mock: {
      from: "Tiếng Việt",
      to: "English",
      input: "Cuộc họp thứ Ba tuần sau dời sang 3 giờ chiều. Vui lòng gửi tài liệu trước thứ Hai.",
      output: EN_OUTPUT,
      chatQuestion: "“circle back” trong cuộc họp mang sắc thái gì?",
      chatAnswer: "Nghĩa là “để sau mình bàn lại” — cách nói nhẹ nhàng để hoãn quyết định mà không nghe như từ chối.",
      chatFollowUp: "Còn cách nói trang trọng hơn?",
      memo: "Gửi báo giá v2 trước thứ Sáu",
      ago2: "2 phút trước",
      ago5: "5 phút trước",
      resetFive: "reset sau khoảng 2 giờ 40 phút",
      resetWeek: "reset sau khoảng 3 ngày",
      signedIn: "Đã đăng nhập",
      models: "hơn 360 mô hình",
    },
  },
}
