# Claude Buddy: thú cưng trên màn hình cho Claude Code

[![Latest release](https://img.shields.io/github/v/release/taivt0109/claude-buddy)](https://github.com/taivt0109/claude-buddy/releases/latest)
[![Downloads](https://img.shields.io/github/downloads/taivt0109/claude-buddy/total)](https://github.com/taivt0109/claude-buddy/releases)
[![Platforms](https://img.shields.io/badge/platform-Windows%20%7C%20macOS-blue)](#cài-đặt)
[![Built with Rust](https://img.shields.io/badge/built%20with-Rust-orange)](https://www.rust-lang.org/)
[![License: MIT](https://img.shields.io/badge/license-MIT-green)](LICENSE)

[English](README.md) | **Tiếng Việt**

Nhân vật nhỏ nổi trên màn hình, cho biết các phiên Claude Code đang làm gì: đang chạy, chờ bạn, đã xong hay bị lỗi. Chạy
trên Windows 10/11 và macOS 11 trở lên.

Chạy nhiều phiên Claude Code cùng lúc mà không phải mở từng terminal xem phiên nào đang chờ mình. Claude Buddy theo dõi
hook của [Claude Code](https://docs.anthropic.com/en/docs/claude-code), báo khi một phiên cần bạn trả lời hoặc đã xong, và
bấm một lần là tới đúng cửa sổ terminal hay IDE (VS Code, Cursor, Windows Terminal…). Đây là app native viết bằng Rust,
nhẹ, không dùng Electron.

## Tính năng

- **Xem nhanh các phiên:** nhân vật cho biết Claude đang chạy, cần bạn, đã xong hay gặp lỗi. Bấm vào để tới đúng cửa sổ của
  phiên đó.
- **Tạo nhân vật từ mô tả:** Codex vẽ đủ 5 trạng thái dạng ảnh động, có cả xoay người và ăn mừng khi xong việc. Không có Codex thì dùng
  ChatGPT rồi kéo file zip vào app.
- **Sửa bằng prompt:** chỉ cần gõ "đội thêm mũ đỏ" là nhân vật được vẽ lại, và Codex nhớ những gì đã vẽ.
- **Chia sẻ nhân vật:** gói trong một file `.buddy`, bấm đúp là thêm được.
- **Tự thêm nhân vật:** WebM, MP4, PNG, APNG, GIF, WebP, JPG hoặc SVG.
- **Mức dùng token:** hôm nay, tuần này, tháng này và 14 ngày gần nhất.
- **Không làm phiền:** tự ẩn khi toàn màn hình, và bấm vào nhân vật không lấy focus khỏi chỗ bạn đang gõ.
- **Tiếng Anh và tiếng Việt.**

## Cài đặt

### Windows

Mở PowerShell, không cần quyền admin.

```powershell
irm https://raw.githubusercontent.com/taivt0109/claude-buddy/main/install.ps1 | iex
```

Lệnh này tải app, tạo shortcut trong Start Menu, thêm hook vào Claude Code rồi mở app. `settings.json` được sao lưu trước
và các hook khác giữ nguyên. Chỉ các phiên Claude Code mở sau đó mới hiện lên.

Chạy lại lệnh này để cập nhật. Không muốn tự thêm hook thì đặt `$env:CLAUDE_BUDDY_NO_HOOKS = "1"` trước khi cài; khi đó
app sẽ hỏi lúc khởi động.

App chưa được ký số, nên Windows SmartScreen hoặc phần mềm diệt virus có thể cảnh báo.

### macOS

Mở Terminal. Chạy trên chip Apple và Intel.

```sh
curl -fsSL https://raw.githubusercontent.com/taivt0109/claude-buddy/main/install.sh | sh
```

Lệnh này cài `Claude Buddy.app` vào `~/Applications`, thêm hook vào Claude Code rồi mở app. `settings.json` được sao lưu
trước và các hook khác giữ nguyên. Chỉ các phiên Claude Code mở sau đó mới hiện lên. Chạy lại lệnh này để cập nhật.

App chưa được Apple ký, nên macOS có thể từ chối mở bản tải bằng trình duyệt. Dùng lệnh trên thì không gặp cảnh báo này.

## Cập nhật

App đã cài tự cập nhật: kiểm tra bản mới nhất khi khởi động và 30 phút một lần, tải về, kiểm tra mã checksum, rồi khởi động
lại sang bản mới ngay khi cửa sổ chính đang đóng. Các phiên Claude Code vẫn chạy bình thường, và mỗi phiên hiện lại khi có
hoạt động tiếp theo. Tắt trong Cài đặt > Tự động cập nhật. Chạy lại lệnh cài cũng cập nhật được. Bản tự build từ mã nguồn không bị đụng tới.

## Gỡ

### Windows

```powershell
irm https://raw.githubusercontent.com/taivt0109/claude-buddy/main/uninstall.ps1 | iex
```

Lệnh này xoá app, shortcut, mục khởi động cùng Windows và hook. Cài đặt, số liệu và nhân vật đã thêm vẫn nằm ở
`%APPDATA%\claude-buddy-rs`, trừ khi bạn đặt `$env:CLAUDE_BUDDY_PURGE = "1"` trước.

### macOS

```sh
curl -fsSL https://raw.githubusercontent.com/taivt0109/claude-buddy/main/uninstall.sh | sh
```

Lệnh này xoá app, mục mở khi đăng nhập và hook. Cài đặt, số liệu và nhân vật đã thêm vẫn nằm ở
`~/Library/Application Support/claude-buddy-rs`; muốn xoá hẳn thì xoá luôn thư mục đó.

## Dùng

| Thao tác | Kết quả |
|---|---|
| Rê chuột vào nhân vật | Hiện đầy đủ trạng thái các phiên |
| Bấm nhân vật | Mở/đóng danh sách phiên (tự đóng sau khi chuột rời đi) |
| Bấm bong bóng | Mở cửa sổ của phiên cần chú ý nhất |
| Bấm một dòng phiên | Mở cửa sổ terminal/IDE của phiên đó |
| Kéo nhân vật | Tới bất kỳ đâu trên màn hình; vị trí được ghi nhớ |
| Lăn chuột trên nhân vật (Windows) | Phóng to hoặc thu nhỏ, tối đa 60% màn hình |
| Trỏ vào nhân vật rồi bấm Ctrl+Q / Ctrl+W / Ctrl+R (Windows) | Skill: dịch chuyển sang góc khác / phân thân ở các góc / cỡ lớn nhất và quay lại. Ở nơi khác thì thêm Alt (Ctrl+Alt+Q…). Sau mỗi skill, phím Ctrl vẫn thuộc về buddy thêm 4 giây, nên bấm tiếp không cần trỏ lại. Đổi phím: mục `skillKeys` ở phần Cấu hình |
| Chuột phải nhân vật hoặc icon khay | Menu nhanh |
| Bấm icon khay (Windows) | Mở cửa sổ chính (Nhân vật, Mức dùng, Cài đặt) |
| Bấm icon thanh menu (macOS) | Menu nhanh, mở cửa sổ chính bằng **Quản lý nhân vật**, **Xem mức dùng** và **Cài đặt** |
| Bấm **Chia sẻ** ở nhân vật bạn đã thêm | Lưu thành file `.buddy` |
| Bấm đúp file `.buddy`, hoặc kéo nó hay file zip vào cửa sổ chính | Thêm nhân vật đó |

Đóng cửa sổ chính thì app vẫn chạy dưới khay hoặc trên thanh menu. Thoát hẳn bằng menu chuột phải.

Giao diện có tiếng Anh và tiếng Việt, mặc định theo hệ thống; đổi trong Cài đặt > Ngôn ngữ.

### Khác biệt trên macOS

- Mở một phiên sẽ đưa terminal hoặc IDE của phiên đó ra trước, nhưng chưa chọn được đúng cửa sổ nếu chương trình có nhiều cửa sổ.
- Chưa có nền cửa sổ trong mờ.
- Chưa có lăn chuột để đổi cỡ, skill và `autoSkills`.
- Thông báo được gửi qua công cụ script của hệ thống, nên macOS có thể hỏi quyền hoặc hiện tên công cụ đó.

## Không che và không làm phiền

- Bong bóng chỉ hiện vài giây khi trạng thái đổi, rộng vừa đủ nội dung. Chỉ phiên cần bạn hoặc bị lỗi mới ở lại.
- Icon khay hoặc thanh menu có chấm màu theo trạng thái, nên xem nhanh được mà không cần gì trên màn hình.
- Vùng trong suốt của cửa sổ buddy cho chuột đi xuyên qua; bấm vào buddy không lấy focus khỏi app đang gõ.
- Tự ẩn khi trình chiếu, xem phim hay chơi game toàn màn hình, rồi tự hiện lại.
- Không kêu và không hiện thông báo khi đang toàn màn hình hoặc bật Focus Assist trên Windows, hoặc khi bạn đang ở đúng cửa sổ
  của phiên đó (trên macOS là đúng chương trình).
- Nhân vật có ba cỡ. Những thứ trên chỉnh trong Cài đặt.

## Mức dùng

Hôm nay, tuần này (từ thứ Hai), tháng này và 14 ngày gần nhất, theo giờ máy. Số liệu đếm từ transcript trong
`.claude/projects` ở thư mục người dùng (cả subagent), chỉ đọc các con số `usage`, không đọc nội dung hội thoại. Số lớn là
đầu vào + đầu ra + ghi cache; đọc cache tính riêng. Không gồm Claude dùng trên máy khác hay trên web.

## Tạo nhân vật bằng Codex

Nếu máy đã cài và đăng nhập [Codex](https://openai.com/codex/), bấm **Tạo bằng Codex** ở trang Nhân vật rồi mô tả nhân vật
bạn muốn. Mất khoảng 6 đến 10 phút và dùng hạn mức Codex của bạn. Bạn sẽ có đủ 5 trạng thái dạng ảnh động, và có thể làm việc
khác trong lúc chờ, miễn là không đóng cửa sổ app. Codex chỉ vẽ, còn app tự kiểm tra và cắt ảnh.

Muốn đổi sau này, chọn nhân vật, bấm **Sửa bằng Codex…** rồi nói bạn muốn đổi gì, ví dụ "đội thêm mũ đỏ". Codex tiếp tục đúng
cuộc trò chuyện đã vẽ nhân vật và vẽ lại đủ 5 trạng thái. Nhân vật chỉ thay đổi khi đã xong cả 5.

Không có Codex? Gõ mô tả, bấm **Sao chép prompt cho ChatGPT**, dán vào ChatGPT, rồi kéo file zip nhận được vào cửa sổ app.

## Chia sẻ nhân vật

Chọn một nhân vật bạn đã thêm rồi bấm **Chia sẻ** để lưu thành một file `.buddy`, sau đó gửi file này cho bất kỳ ai. Để
thêm nhân vật người khác gửi, bấm đúp vào file hoặc kéo vào cửa sổ app, không cần khởi động lại. Nếu đã có nhân vật trùng
tên, app giữ cả hai.

File `.buddy` chỉ chứa ảnh, video, âm thanh và phần mô tả. App không chạy bất cứ thứ gì trong file, và từ chối file có nội
dung khác.

## Tự thêm nhân vật

Thêm nhân vật từ file hoặc nhập cả thư mục (`character.json` và một file media cho mỗi trạng
thái):

| Định dạng | Ghi chú |
|---|---|
| WebM VP9 có alpha | Nền trong suốt |
| MP4 | Không có nền trong suốt |
| PNG, APNG, GIF, WebP, JPG | Ảnh nền trắng có thể tách nền khi thêm |
| SVG | Vẽ như ảnh tĩnh, hiệu ứng trong file không chạy |

Âm thanh riêng: `sounds/done` và `sounds/waiting` dạng `.wav` hoặc `.mp3`. Clip rất dài bị cắt bớt (giới hạn 50 MB mỗi file).

Trên Windows, ảnh lớn hơn cỡ thường (ví dụ 1024 pixel) vẫn nét khi phóng to nhân vật.

Trên Windows, thêm `"autoSkills": true` vào `character.json` thì nhân vật tự dùng skill:

| Khi | Skill |
|---|---|
| Các phiên làm việc được 20 giây | Phân thân xuất hiện, tan khi hết việc |
| Một phiên cần bạn | Dịch chuyển tới góc gần con trỏ chuột nhất |
| Các phiên xong việc | Dịch chuyển sang góc khác |

Không tự dùng khi Ctrl+R đang để ở cỡ lớn nhất, khi khoá màn hình, hoặc khi bạn đang kéo nhân vật. Bật hoặc tắt cho nhân vật bất kỳ
trong menu chuột phải (**Tự dùng skill theo phiên**).

## Cấu hình

`config.json` trong `%APPDATA%\claude-buddy-rs` trên Windows hoặc `~/Library/Application Support/claude-buddy-rs` trên
macOS, mục `port` (mặc định 47329). Đổi cổng thì cần kết nối lại Claude Code.
Trên Windows, mục `autoCloneSeconds` (mặc định 20) là số giây các phiên làm việc trước khi phân thân xuất hiện.
Mục `skillKeys` đặt phím cho từng skill, mỗi skill một chữ hoặc một số, `""` là không dùng phím:
`{"blink": "Q", "clones": "W", "ultimate": "R"}`. Ctrl và Ctrl+Alt giữ nguyên; sửa xong thì mở lại app.
Công tắc tự dùng skill cũng có trong trang **Cài đặt**.

## API cục bộ

Chỉ nghe trên `127.0.0.1`. Từ chối request có header `Origin` hoặc `Host` lạ, để trang web không điều khiển được app.

- `POST /hook`: dữ liệu hook thô của Claude Code
- `POST /event`: sự kiện từ công cụ khác, ví dụ `{"session_id":"a","state":"working","step":"test","label":"Backend"}`
- `GET /sessions`, `GET /usage`, `GET /health`

## Chạy từ mã nguồn

```
cargo build --release
target\release\claude-buddy-rs.exe     # Windows
target/release/claude-buddy-rs         # macOS
```

Cần Rust, cộng với Windows SDK trên Windows (để nhúng icon) hoặc Xcode Command Line Tools trên macOS. Lần đầu app hỏi có
kết nối với Claude Code không. Nếu đồng ý, app thêm hook của riêng nó vào `.claude/settings.json` ở thư mục người dùng và
giữ bản sao lưu `settings.json.buddy-rs-backup-*`.

`scripts/package-macos.sh` đóng gói app macOS từ bản release cho cả hai loại chip.

Hook lưu đường dẫn đầy đủ tới chương trình. Nếu chuyển nó đi nơi khác, hãy kết nối lại trong Cài đặt. Nếu tự xoá nó, hãy tắt "Kết nối Claude Code" trước (hoặc chạy `--remove-hooks`), nếu không Claude Code sẽ báo lỗi hook.

## Phát triển

```
cargo test
set PREVIEW_DIR=C:\some\folder && cargo test preview -- --ignored
cargo test export_app_icon -- --ignored
```

Lệnh thứ hai xuất ảnh PNG của nhân vật và cửa sổ. Lệnh thứ ba tạo lại `assets/app.ico` từ logo vẽ trong code.

Biến môi trường để kiểm tra mà không đụng dữ liệu thật: `CLAUDE_BUDDY_RS_HOME` (thư mục dữ liệu riêng, chạy song song
được), `CLAUDE_CONFIG_DIR` (thư mục Claude), `CLAUDE_BUDDY_RS_THEME=dark|light`, `CLAUDE_BUDDY_RS_BACKDROP=blur`,
`CLAUDE_BUDDY_RS_KEEP_ANIMATING=1` (Windows: không tạm dừng khi khoá máy, để đo CPU),
`CLAUDE_BUDDY_RS_OPEN_PAGE=0|1|2` (macOS: mở cửa sổ chính khi khởi động), `CLAUDE_BUDDY_RS_UPDATE_URL` cùng
`CLAUDE_BUDDY_RS_UPDATE_FROM` (cập nhật từ server thử cục bộ, giả làm phiên bản đó).

## Giấy phép

MIT, xem [LICENSE](LICENSE).
