# Vendored FFmpeg patch: D3D11VA HEVC 4:4:4 (Range Extensions)

## ffmpeg-d3d11va-hevc-444.patch

Stock FFmpeg cannot hardware-decode HEVC 4:4:4 (RExt) through D3D11VA. Three
independent gates block it, all verified against FFmpeg 8.1, 9.0.2 and master
`dc52424419`:

1. `libavcodec/hevc/hevcdec.c:get_format()` only offers `AV_PIX_FMT_D3D11` /
   `AV_PIX_FMT_D3D11VA_VLD` for 4:2:0 SPS formats — 4:4:4 streams fall back to
   software.
2. `libavcodec/dxva2.c:dxva_modes[]` lists only the HEVC Main/Main10 decoder
   GUIDs, so the profile-4 stream never matches a Range Extensions GUID.
3. `libavcodec/dxva2_hevc.c` submits the base `DXVA_PicParams_HEVC`
   (232 bytes); a RExt decoder GUID requires
   `DXVA_PicParams_HEVC_RangeExt` (Windows SDK `dxva.h`, 10.0.22621+), and
   `SubmitDecoderBuffers` rejects the short buffer with `0x80070057`
   (E_INVALIDARG).

The patch adds the two RExt mode GUIDs (`Main_444` = `4008018f-…`,
`Main10_444` = `0dabeffa-…`, both advertised by the RTX 3080 driver), offers
the D3D11 formats for `YUV444P`/`YUV444P10`, tracks the selected decoder GUID
in `FFDXVASharedContext`, and submits the full Range Extensions picture
parameters (SPS/PPS range-extension flags and fields) for RExt GUIDs while
keeping the base-size submission for Main/Main10.

Apply with `patch -p1` inside the FFmpeg source tree before configuring.

## SDK build (deps/ffmpeg-444-build)

Source: FFmpeg master commit `dc52424419` (the commit behind BtbN's
`ffmpeg-master-latest-win64-lgpl-shared`), cross-compiled with mingw-w64
(`gcc-mingw-w64-x86-64`, `nasm`) with the patch applied. NVIDIA support comes
from nv-codec-headers tag `n9.1.23.3` installed into the same prefix
(`https://github.com/FFmpeg/nv-codec-headers`,
`make install PREFIX=<prefix>`) so `ffnvcodec.pc` is visible to configure; a
`x86_64-w64-mingw32-pkg-config` wrapper restricts pkg-config to that prefix
so host Linux libraries can never leak into the cross build.

Full configure line (reproducible):

```sh
./configure \
  --prefix=/mnt/c/Users/necba/opennow-444-lab/deps/ffmpeg-444-build \
  --target-os=mingw32 --arch=x86_64 --cross-prefix=x86_64-w64-mingw32- \
  --enable-shared --disable-static --disable-programs --disable-doc \
  --enable-ffnvcodec --enable-nvdec --enable-cuvid \
  --enable-d3d11va --enable-dxva2
make -j12 && make install
```

The build produces LGPL shared libraries (`avcodec-63.dll`, `avutil-61.dll`,
`avformat-63.dll`, `avfilter-12.dll`, `avdevice-63.dll`, `swresample-7.dll`,
`swscale-10.dll`) plus MSVC-compatible `*.lib` import libraries and `*.def`
files. `Build-Bridge.ps1` points `OPENNOW_FFMPEG_DIR` at this prefix; the
previous stock SDK (`deps/ffmpeg-n8.1-latest-win64-lgpl-shared-8.1`) remains
the rollback target.

Runtime note: the patched build is compiled by mingw and therefore also ships
`libgcc_s_seh-1.dll` and `libwinpthread-1.dll`, which must sit next to
`avcodec-63.dll` in `app\bin`.
