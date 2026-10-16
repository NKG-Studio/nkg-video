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

## GPUImage shader formulas

The exposure adjustment in src/alpha.wgsl is adapted from BradLarson/GPUImage,
revision 167b0389bc6e9dc4bb0121550f91d8d5d6412c53:
https://github.com/BradLarson/GPUImage/tree/167b0389bc6e9dc4bb0121550f91d8d5d6412c53/framework/Source

Source: GPUImageExposureFilter.m.
Port changes: WGSL exposure with explicit sRGB conversion and linear-light
alpha premultiplication. The framework itself is not bundled.

Copyright (c) 2012, Brad Larson, Ben Cochran, Hugues Lismonde, Keitaroh Kobayashi, Alaric Cole, Matthew Clark, Jacob Gundersen, Chris Williams.
All rights reserved.

Redistribution and use in source and binary forms, with or without modification, are permitted provided that the following conditions are met:

Redistributions of source code must retain the above copyright notice, this list of conditions and the following disclaimer.
Redistributions in binary form must reproduce the above copyright notice, this list of conditions and the following disclaimer in the documentation and/or other materials provided with the distribution.
Neither the name of the GPUImage framework nor the names of its contributors may be used to endorse or promote products derived from this software without specific prior written permission.
THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO, THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL, EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO, PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

## Lucide status icons

clock, film, timer and monitor from Lucide 0.468.0:
https://github.com/lucide-icons/lucide/tree/0.468.0/icons

Original SVG files are preserved in assets/lucide. Their geometry is adapted to
epaint primitives in src/ui.rs without adding an SVG runtime dependency.

ISC License

Copyright (c) for portions of Lucide are held by Cole Bemis 2013-2022 as part of Feather (MIT). All other copyright (c) for Lucide are held by Lucide Contributors 2022.

Permission to use, copy, modify, and/or distribute this software for any
purpose with or without fee is hereby granted, provided that the above
copyright notice and this permission notice appear in all copies.

THE SOFTWARE IS PROVIDED "AS IS" AND THE AUTHOR DISCLAIMS ALL WARRANTIES
WITH REGARD TO THIS SOFTWARE INCLUDING ALL IMPLIED WARRANTIES OF
MERCHANTABILITY AND FITNESS. IN NO EVENT SHALL THE AUTHOR BE LIABLE FOR
ANY SPECIAL, DIRECT, INDIRECT, OR CONSEQUENTIAL DAMAGES OR ANY DAMAGES
WHATSOEVER RESULTING FROM LOSS OF USE, DATA OR PROFITS, WHETHER IN AN
ACTION OF CONTRACT, NEGLIGENCE OR OTHER TORTIOUS ACTION, ARISING OUT OF
OR IN CONNECTION WITH THE USE OR PERFORMANCE OF THIS SOFTWARE.

The MIT License (MIT)

Copyright (c) 2013-2023 Cole Bemis

Permission is hereby granted, free of charge, to any person obtaining a copy
of this software and associated documentation files (the "Software"), to deal
in the Software without restriction, including without limitation the rights
to use, copy, modify, merge, publish, distribute, sublicense, and/or sell
copies of the Software, and to permit persons to whom the Software is
furnished to do so, subject to the following conditions:

The above copyright notice and this permission notice shall be included in all
copies or substantial portions of the Software.

THE SOFTWARE IS PROVIDED "AS IS", WITHOUT WARRANTY OF ANY KIND, EXPRESS OR
IMPLIED, INCLUDING BUT NOT LIMITED TO THE WARRANTIES OF MERCHANTABILITY,
FITNESS FOR A PARTICULAR PURPOSE AND NONINFRINGEMENT. IN NO EVENT SHALL THE
AUTHORS OR COPYRIGHT HOLDERS BE LIABLE FOR ANY CLAIM, DAMAGES OR OTHER
LIABILITY, WHETHER IN AN ACTION OF CONTRACT, TORT OR OTHERWISE, ARISING FROM,
OUT OF OR IN CONNECTION WITH THE SOFTWARE OR THE USE OR OTHER DEALINGS IN THE
SOFTWARE.
