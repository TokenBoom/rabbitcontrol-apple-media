# RabbitControl Apple Media(`TokenBoom/rabbitcontrol-apple-media`)

RabbitControl 的 Apple 平台(iOS/macOS)媒体库:**VideoToolbox 硬件 H.264 解码 + 帧泵渲染**,
以 C ABI FFI 暴露给移动端/桌面客户端。独立于 [`rabbitcontrol-core`](https://github.com/TokenBoom/rabbitcontrol-core),
经 core 的平台注册接口(`PlatformMediaProvider` / `ClientPlatformHooks`)单向接入,core 不依赖本库。

## 仓库结构

| Crate | 职责 |
|---|---|
| `rc-decode-vt` | VideoToolbox 硬件 H.264 解码后端(macOS+iOS),移植自 rabbitcontrol-macos |
| `rc-media-apple` | 注册胶水 + 帧泵渲染器 + C ABI FFI(`rc_apple_media_init/frame_take/frame_release`) |

## 帧所有权契约

`FramePumpRenderer` 把每帧 `CVPixelBuffer` retain 进全局槽;客户端 `rc_apple_media_frame_take`
取走(所有权转移),用毕必须 `rc_apple_media_frame_release`。v1 全局单帧槽,假定单会话。

## 本地构建

依赖相邻目录的 `../rabbitcontrol-core` 克隆(path 依赖;上游发布 tag 后可切 git 依赖):

```sh
cargo build -p rc-media-apple --target aarch64-apple-ios-sim
```
