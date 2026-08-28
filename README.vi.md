<div align="center">

# SayKnow Kit

**Bộ công cụ AI trên thanh menu — dịch, chat và clipboard trong một cửa sổ.**

`say` (nói) + `know` (biết) — nói là hiểu.

[한국어](README.ko.md) · [English](README.md) · [日本語](README.ja.md) · [中文](README.zh.md) · [Español](README.es.md) · [Français](README.fr.md) · [Deutsch](README.de.md) · **Tiếng Việt**

[![macOS](https://img.shields.io/badge/macOS-11.0%2B-black?logo=apple)](https://www.apple.com/macos/)
[![Tauri](https://img.shields.io/badge/Tauri-2-FFC131?logo=tauri&logoColor=black)](https://tauri.app)
[![License: MIT](https://img.shields.io/badge/License-MIT-green.svg)](LICENSE)

</div>

---

## Tổng quan

SayKnow Kit nằm trên **thanh menu macOS**. Một phím tắt mở popup chứa ba công cụ bạn dùng mỗi ngày: **dịch** (tự chạy khi bạn ngừng gõ), **chat** và **lịch sử clipboard**. Không cần chuyển tab giữa trang dịch và copy-paste qua lại nữa.

**Ba nhà cung cấp** trong cùng một cửa sổ (OpenRouter / OCP / endpoint tùy chỉnh). Chỉ riêng OpenRouter đã cho phép dùng hơn 360 model (GPT-4o, Claude, Gemini, Llama,...) bằng một khóa duy nhất.

## Tính năng

- 🌞 **Tab công cụ** — điều khiển độ sáng mọi màn hình đã kết nối ở cấp phần cứng (DDC/CI cho màn ngoài, IOKit cho màn tích hợp nếu được phép) và bật/tắt màn ngoài (chờ DDC)
- 🌞 **Tab công cụ** — điều khiển độ sáng mọi màn hình đã kết nối ở cấp phần cứng (DDC/CI cho màn ngoài, IOKit cho màn tích hợp nếu được phép) và bật/tắt màn ngoài (chờ DDC)
- 📊 **Tab mức dùng** — đọc trực tiếp nhật ký phiên mà Claude Code, Codex và SayKnow CLI đã ghi cục bộ. Hiển thị khối tính phí 5 giờ (thời gian còn lại, tốc độ tiêu thụ) cùng phần trăm 5 giờ và hàng tuần thật do CLI ghi lại, kèm giờ reset. Cửa sổ đã reset bị gạch ngang và không bao giờ được vẽ như mức hiện tại. Không cần mạng, không cần đăng nhập thêm
- 📋 **Tab lịch sử clipboard** — thu thập nền, tìm kiếm cả nội dung lẫn ghi chú, ghim, gửi sang dịch và xoá hai mức. Chuỗi rỗng, dạng OTP và khối khoá PEM không bao giờ được lưu
- 💬 **Tab Chat** — hỏi-đáp gọn trong cùng cửa sổ, thanh bên đa cuộc trò chuyện, theo từng tin nhắn: tạo lại / chỉnh sửa / sao chép / dừng
- 🤖 **Đa nhà cung cấp** — OpenRouter / OCP / bất kỳ endpoint nào tương thích OpenAI
- 📦 **Cài OCP một chạm** — ứng dụng tự chạy `git clone → npm install → setup.mjs`, hiển thị log trực tiếp
- 📐 **Chế độ ngang gọn** — 720×240 hai cột, phù hợp để mở thường trực
- 🪟 **Chuyển kích thước cửa sổ** — Gọn ↔ Bình thường ngay tại thanh tiêu đề
- ⚡ **Tự động dịch** — sau 1.5 giây ngừng gõ
- ⌨️ **Chế độ thủ công** — chỉ khi nhấn `⌘⏎` hoặc nút Dịch
- 🪄 **Tinh chỉnh** — Lịch sự / Thân mật / Ngắn / Công sở / Sát nghĩa + prompt tự do
- 🌐 **OpenRouter BYOK** — combobox tìm kiếm 360+ model
- 🔁 **Model dự phòng** — OpenRouter tự chuyển khi model chính lỗi
- ⏹ **Dừng** — hủy cuộc gọi đang chạy
- 📋 **Tự lấy clipboard** — `⌘⇧T` tự điền text vào ô nhập
- 🕘 **Lịch sử** — tìm kiếm, ghim mục để giữ lại vĩnh viễn
- 📌 **Ghim cửa sổ** — tắt tự ẩn
- 📚 **Thuật ngữ** (Glossary) — dịch nhất quán tên riêng
- ✏️ **Tùy chỉnh system prompt**
- 💰 **Theo dõi chi phí** — token và tiền theo ngày/tháng
- 🌓 **Sáng / tối / hệ thống**
- 🌍 **8 ngôn ngữ giao diện** — tự nhận diện
- 🔄 **36 ngôn ngữ dịch**
- 🔒 **macOS Keychain** — API key mã hóa AES-256

## Yêu cầu hệ thống

- macOS 11.0 (Big Sur) trở lên
- Apple Silicon (aarch64)
- OpenRouter API key — lấy tại [openrouter.ai/keys](https://openrouter.ai/keys)

## Cài đặt

### Cách 1 — DMG đóng gói sẵn (khuyến nghị)

1. Tải `SayKnow Kit_x.x.x_aarch64.dmg` từ [Releases](https://github.com/jaybeyond/sayknow-kit/releases).
2. Mở DMG, kéo SayKnow Kit.app vào `/Applications`.
3. Build chưa code-sign nên Gatekeeper sẽ chặn lần đầu:
   ```bash
   xattr -dr com.apple.quarantine "/Applications/SayKnow Kit.app"
   ```

### Cách 2 — Build từ source

```bash
git clone https://github.com/jaybeyond/sayknow-kit.git
cd sayknow-kit
pnpm install
pnpm tauri build
```

## Sử dụng

1. Một icon nhỏ xuất hiện ở thanh menu (cạnh đồng hồ). Không hiện trong Dock.
2. Bấm icon → nhập OpenRouter API key → **Kết nối & bắt đầu**.
3. Key được lưu vào macOS Keychain.
4. Bấm icon hoặc `⌘⇧T` → gõ → tự dịch sau 1.5 giây.

### Phím tắt

| Phím tắt | Hành động |
|---|---|
| `⌘⇧T` | Mở/đóng popup (toàn cục) |
| `⌘⏎` | Dịch ngay (chế độ thủ công) |

### Cài đặt (cửa sổ riêng)

⚙️ → **Cài đặt** mở cửa sổ có sidebar:
- **Chung** — chế độ, clipboard, ghim, giao diện, ngôn ngữ
- **Kết nối** — model chính/dự phòng, đăng xuất
- **Thuật ngữ** — cặp từ
- **System prompt** — chỉnh sửa prompt dịch/tinh chỉnh
- **Sử dụng** — token và chi phí
- **Thông tin**

## Bảo mật

Key OpenRouter là thông tin tính phí, SayKnow Kit không bao giờ lưu plaintext:

- macOS **Keychain** (`com.sayknow.app`)
- Mã hóa AES-256, key dẫn xuất từ mật khẩu đăng nhập macOS
- Ứng dụng khác cố đọc sẽ kích hoạt prompt Cho phép/Từ chối của hệ thống

## Giấy phép

[MIT](LICENSE) — chi tiết xem [README tiếng Anh](README.md)
