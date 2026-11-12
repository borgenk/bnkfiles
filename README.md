# BNK Files

[![CI](https://github.com/borgenk/bnkfiles/actions/workflows/ci.yml/badge.svg)](https://github.com/borgenk/bnkfiles/actions/workflows/ci.yml)
[![Release](https://img.shields.io/github/v/release/borgenk/bnkfiles)](https://github.com/borgenk/bnkfiles/releases/latest)

A native Wayland + Vulkan file explorer with a built-in image viewer, written in
Rust: no toolkit, and no dependencies beyond the image decoders.

> [!WARNING]
> Do not use bnkfiles yet. Its file operations are not properly tested.

_Disclaimer: personal project, built mainly for my own use, AI-assisted._

## Requirements

- A Linux desktop running a **Wayland** session
- A compositor that supports **xdg_wm_base** and **zwp_linux_dmabuf_v1** version 4
- A working **Vulkan** device
- The freetype, harfbuzz, fontconfig and libxkbcommon libraries

## Install

```sh
curl -fsSL https://raw.githubusercontent.com/borgenk/bnkfiles/main/install.sh | sh
```

This drops the binary in `~/.local/bin` and installs the desktop entry.
