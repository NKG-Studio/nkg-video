# Third-party components

NKG Video is distributed under GPL-3.0-or-later (see LICENSE).

The Windows archive includes unmodified FFmpeg 8.1.2 full shared binaries from
[GyanD/codexffmpeg](https://github.com/GyanD/codexffmpeg/releases/tag/8.1.2).
Their GPL v3 license is in FFmpeg-LICENSE.txt. FFmpeg-README.txt contains the
upstream source revision, build configuration, and external library versions.
FFmpeg source: https://github.com/FFmpeg/FFmpeg/tree/38b88335f9
Build provider and source information: https://www.gyan.dev/ffmpeg/builds/
The exact downloaded SDK and checksum are recorded in scripts/setup-ffmpeg-native.ps1.

Rust dependency license files and package/source metadata are included under
licenses/ in the binary archive. Cargo.lock pins their versions. Third-party
components retain their respective licenses and copyright notices.

Microsoft Visual C++ runtime DLLs are redistributed from the Visual Studio
redistributable directory and remain subject to Microsoft's terms:
https://visualstudio.microsoft.com/license-terms/

Application source and build instructions:
https://github.com/NKG-Studio/nkg-video
