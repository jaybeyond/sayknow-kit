<div align="center">

<img src="public/sayo-logo.png" alt="Sayo, linh vật bạch tuộc của SayKnow Kit" width="180" />

# SayKnow Kit

**Bộ công cụ AI trên thanh menu — dịch, chat và clipboard trong một cửa sổ.**

`say` (nói) + `know` (biết) — nói là hiểu.

[한국어](README.ko.md) · [English](README.md) · [日本語](README.ja.md) · [中文](README.zh.md) · [Español](README.es.md) · [Français](README.fr.md) · [Deutsch](README.de.md) · **Tiếng Việt**

[![macOS](https://img.shields.io/badge/macOS-11.0%2B-black?logo=apple)](https://www.apple.com/macos/) · [![Windows](https://img.shields.io/badge/Windows-10%2F11-blue?logo=windows)](https://www.microsoft.com/windows/)
[![Tauri](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=black)](https://tauri.app)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

</div>

---

## Tổng quan

SayKnow Kit hoạt động trên macOS và Windows: xuất hiện ở **thanh menu macOS** hoặc **khay hệ thống Windows**. Phím tắt mở popup chứa ba công cụ hằng ngày: **dịch**, **chat** và **lịch sử clipboard**.

**Ba nhà cung cấp** trong cùng một cửa sổ (OpenRouter / OCP / endpoint tùy chỉnh). Chỉ riêng OpenRouter đã cho phép dùng hơn 360 model (GPT-4o, Claude, Gemini, Llama,...) bằng một khóa duy nhất.

## Tính năng

- 🌞 **Tab công cụ** — trên macOS điều khiển độ sáng (DDC/CI cho màn ngoài; IOKit cho đèn nền màn tích hợp nếu được hỗ trợ). Trên các máy Mac mới được hỗ trợ nhưng không có IOKit, ứng dụng dùng tự động hóa Trợ năng của Trung tâm điều khiển. IOKit và tự động hóa này chỉ dành cho macOS; khả năng DDC tùy phần cứng.
- 📊 **Trạng thái hệ thống** — CPU, bộ nhớ và ổ đĩa hệ thống; nhiệt độ gói CPU hiển thị “không khả dụng” trong v0.2.6 vì chưa có bộ chuyển đổi nhiệt độ được xác minh, cũng như trên hệ thống không hỗ trợ.
- 📊 **Tab mức dùng** — đọc trực tiếp nhật ký phiên mà Claude Code, Codex và SayKnow CLI đã ghi cục bộ. Hiển thị khối tính phí 5 giờ (thời gian còn lại, tốc độ tiêu thụ) cùng phần trăm 5 giờ và hàng tuần thật do CLI ghi lại, kèm giờ reset. Cửa sổ đã reset bị gạch ngang và không bao giờ được vẽ như mức hiện tại. Không cần mạng, không cần đăng nhập thêm
- 📋 **Tab lịch sử clipboard** — thu thập nền, tìm kiếm cả nội dung lẫn ghi chú, ghim, gửi sang dịch và xoá hai mức. Chuỗi rỗng, dạng OTP và khối khoá PEM không bao giờ được lưu
- 💬 **Tab Chat** — hỏi-đáp gọn trong cùng cửa sổ, thanh bên đa cuộc trò chuyện, theo từng tin nhắn: tạo lại / chỉnh sửa / sao chép / dừng
- 🤖 **Đa nhà cung cấp** — OpenRouter / OCP / bất kỳ endpoint nào tương thích OpenAI
- 📦 **Cài OCP một chạm** — ứng dụng tự chạy `git clone → npm install → setup.mjs`, hiển thị log trực tiếp
- 📐 **Chế độ ngang gọn** — 720×240 hai cột, phù hợp để mở thường trực
- 🪟 **Chuyển kích thước cửa sổ** — Gọn ↔ Bình thường ngay tại thanh tiêu đề
- ⚡ **Tự động dịch** — sau 1.5 giây ngừng gõ
- ⌨️ **Chế độ thủ công** — chỉ khi nhấn `⌘⏎` (macOS), `Ctrl+Enter` (Windows) hoặc nút Dịch
- 🪄 **Tinh chỉnh** — Lịch sự / Thân mật / Ngắn / Công sở / Sát nghĩa + prompt tự do
- 🌐 **OpenRouter BYOK** — combobox tìm kiếm 360+ model
- 🔁 **Model dự phòng** — OpenRouter tự chuyển khi model chính lỗi
- ⏹ **Dừng** — hủy cuộc gọi đang chạy
- 📋 **Tự lấy clipboard** — `⌘⇧T` (macOS) / `Ctrl+Shift+T` (Windows) tự điền text vào ô nhập
- 🕘 **Lịch sử** — tìm kiếm, ghim mục để giữ lại vĩnh viễn
- 📌 **Ghim cửa sổ** — tắt tự ẩn
- 📚 **Thuật ngữ** (Glossary) — dịch nhất quán tên riêng
- ✏️ **Tùy chỉnh system prompt**
- 💰 **Theo dõi chi phí** — token và tiền theo ngày/tháng
- 🌓 **Sáng / tối / hệ thống**
- 🌍 **8 ngôn ngữ giao diện** — tự nhận diện
- 🔄 **36 ngôn ngữ dịch**
- 🔒 **Lưu trữ an toàn** — thông tin xác thực trong kho hệ thống (macOS Keychain / Windows Credential Manager)

## Yêu cầu hệ thống

- macOS 11.0 (Big Sur) trở lên trên Apple Silicon (aarch64)
- Windows 10/11 x64
- OpenRouter API key — lấy tại [openrouter.ai/keys](https://openrouter.ai/keys)

## Cài đặt

### macOS — DMG

1. Tải DMG macOS aarch64 từ [bản phát hành GitHub chính thức](https://github.com/jaybeyond/sayknow-kit/releases).
2. Mở DMG, kéo SayKnow Kit.app vào `/Applications`.
3. Ứng dụng v0.2.6 chỉ có chữ ký **ad hoc**, không có Developer ID hay notarization; cảnh báo Gatekeeper là điều bình thường.
4. Kiểm tra `SHA256SUMS.txt` được phát hành cùng phiên bản trước khi mở.

### Windows — EXE hoặc MSI

1. Chỉ tải bộ cài NSIS `.exe` hoặc bộ cài MSI `.msi` x64 từ GitHub Release chính thức.
2. Windows SmartScreen có thể cảnh báo vì v0.2.6 chưa được ký; điều này bình thường. Không có Authenticode.
3. Kiểm tra `SHA256SUMS.txt`, sau đó chạy trình cài đặt.
4. Gỡ cài đặt qua **Cài đặt → Ứng dụng → Ứng dụng đã cài đặt → SayKnow Kit → Gỡ cài đặt**.

Trên macOS, gỡ bằng cách xóa SayKnow Kit.app khỏi `/Applications` và xóa dữ liệu Keychain nếu không còn cần.

Chỉ tải từ [GitHub Releases chính thức](https://github.com/jaybeyond/sayknow-kit/releases).

### Cách 2 — Build từ source

```bash
git clone https://github.com/jaybeyond/sayknow-kit.git
cd sayknow-kit
pnpm install
pnpm tauri build
```

## Sử dụng

1. Một biểu tượng xuất hiện ở thanh menu macOS hoặc khay hệ thống Windows và luôn sẵn sàng ở đó.
2. Bấm biểu tượng → nhập OpenRouter API key → **Kết nối & bắt đầu**. Key được lưu trong kho thông tin xác thực hệ thống.
3. Bấm biểu tượng hoặc nhấn `⌘⇧T` (macOS) / `Ctrl+Shift+T` (Windows) để mở cửa sổ → gõ → tự dịch sau 1,5 giây.

### Phím tắt

| Phím tắt | Hành động |
|---|---|
| `⌘⇧T` (macOS) | Mở/đóng cửa sổ (toàn cục) |
| `Ctrl+Shift+T` (Windows) | Mở/đóng cửa sổ (toàn cục) |
| `⌘⏎` (macOS) / `Ctrl+Enter` (Windows) | Dịch ngay (chế độ thủ công) |

### Cài đặt (cửa sổ riêng)

⚙️ → **Cài đặt** mở cửa sổ có sidebar:
- **Chung** — chế độ, clipboard, ghim, giao diện, ngôn ngữ
- **Kết nối** — model chính/dự phòng, đăng xuất
- **Thuật ngữ** — cặp từ
- **System prompt** — chỉnh sửa prompt dịch/tinh chỉnh
- **Sử dụng** — token và chi phí
- **Thông tin**

## Bảo mật

OpenRouter API key là thông tin tính phí và chỉ được lưu trong kho bảo mật hệ thống:

- macOS **Keychain** và Windows **Credential Manager**
- Không lưu plaintext và không dẫn xuất từ mật khẩu đăng nhập
- Chỉ tải từ GitHub Releases chính thức và kiểm tra `SHA256SUMS.txt`
- Ứng dụng macOS v0.2.6 dùng chữ ký ad hoc, không có Developer ID hay notarization; trình cài Windows không có chữ ký Authenticode. Cảnh báo Gatekeeper và SmartScreen là điều bình thường.

## Giấy phép

[MIT](LICENSE) — chi tiết xem [README tiếng Anh](README.md)
