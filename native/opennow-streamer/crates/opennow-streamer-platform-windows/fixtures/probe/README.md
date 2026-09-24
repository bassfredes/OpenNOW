# Windows advanced-format decode probes

These synthetic black access units exercise the actual Media Foundation decoder before
advertising 10-bit, 4:4:4, or HDR support. Each contains one independent 1920x1080 frame
at 60 Hz with no reordering. HEVC uses Annex B; AV1 uses low-overhead OBU framing.
All samples use limited range. SDR is BT.709; HDR10 is BT.2020 nonconstant-luminance/PQ.
They contain no captured user or game content.
HEVC uses `keyint=60` even though only one IDR is emitted: `keyint=1` would select
the intra-only RExt profile instead of Main10 for the P010 samples.

Generated with FFmpeg 6.1.1, libx265 and libaom-av1. FFmpeg is a development-only
fixture generator, not a Windows runtime dependency. From this directory:

```sh
for spec in 'p010 yuv420p10le' 'ayuv yuv444p' 'y410 yuv444p10le'; do
    set -- $spec
    ffmpeg -hide_banner -loglevel error \
        -f lavfi -i 'color=black:size=1920x1080:rate=60' -frames:v 1 \
        -pix_fmt "$2" -c:v libx265 -preset ultrafast \
        -x265-params 'log-level=error:pools=1:frame-threads=1:info=0:keyint=60:bframes=0:repeat-headers=1:colorprim=bt709:transfer=bt709:colormatrix=bt709:chromaloc=0' \
        -f hevc -y "hevc-$1-sdr.hevc"
done
ffmpeg -hide_banner -loglevel error \
    -f lavfi -i 'color=black:size=1920x1080:rate=60' -frames:v 1 \
    -pix_fmt yuv420p10le -c:v libx265 -preset ultrafast \
    -x265-params 'log-level=error:pools=1:frame-threads=1:info=0:keyint=60:bframes=0:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:chromaloc=0' \
    -f hevc -y hevc-p010-pq.hevc
ffmpeg -hide_banner -loglevel error \
    -f lavfi -i 'color=black:size=1920x1080:rate=60' -frames:v 1 \
    -pix_fmt yuv444p10le -c:v libx265 -preset ultrafast \
    -x265-params 'log-level=error:pools=1:frame-threads=1:info=0:keyint=60:bframes=0:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:chromaloc=0' \
    -f hevc -y hevc-y410-pq.hevc
for transfer in sdr pq; do
    if [ "$transfer" = pq ]; then
        colors='-color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc'
    else
        colors='-color_primaries bt709 -color_trc bt709 -colorspace bt709'
    fi
    ffmpeg -hide_banner -loglevel error \
        -f lavfi -i 'color=black:size=1920x1080:rate=60' -frames:v 1 \
        -pix_fmt yuv420p10le -c:v libaom-av1 -cpu-used 8 -threads 2 \
        -lag-in-frames 0 -g 1 $colors -color_range tv \
        -f obu -y "av1-p010-$transfer.obu"
done
```

Verify the encoded dimensions, pixel format, and color metadata with:

```sh
for file in *.hevc *.obu; do
    ffprobe -v error -show_entries \
        stream=codec_name,profile,width,height,pix_fmt,color_space,color_transfer,color_primaries \
        -of compact "$file"
    ffmpeg -v error -i "$file" -frames:v 1 -f null -
done
```

## Non-black hardware precision regression

`hevc-y410-precision.hevc` is a separate lossless Main44410/RExt access unit for the explicit
hardware regression, not a startup capability probe. The first 32 rows contain gray code values
64 through 940, repeating every 877 pixels, with neutral chroma. Remaining rows have Y=512,
alternating U=384/640 and V=640/384 at every pixel. All 6,220,800 decoded Y/U/V samples were
verified against this source using FFmpeg software decoding. The hardware test allows two RGB
code values of error for the ramp and four for the chroma matrix conversion; it does not allow
an 8-bit or chroma-subsampled result.

```sh
ffmpeg -hide_banner -loglevel error -f lavfi \
    -i "nullsrc=size=1920x1080:rate=60,format=yuv444p10le,geq=lum='if(lt(Y,32),64+mod(X,877),512)':cb='if(lt(Y,32),512,if(mod(X,2),640,384))':cr='if(lt(Y,32),512,if(mod(X,2),384,640))'" \
    -frames:v 1 -c:v libx265 -preset ultrafast \
    -x265-params 'log-level=error:pools=1:frame-threads=1:info=0:keyint=60:bframes=0:repeat-headers=1:lossless=1:colorprim=bt709:transfer=bt709:colormatrix=bt709:chromaloc=0' \
    -f hevc -y hevc-y410-precision.hevc
```

## Main44410 PQ probes and hardware precision regression

`hevc-y410-pq-precision.hevc` uses the same lossless 4:4:4 precision source and encoder
options as `hevc-y410-precision.hevc`, replacing `colorprim=bt709:transfer=bt709:colormatrix=bt709`
with `colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc`. The HDR test keeps the same
decoder precision, chroma, restart, and output precision assertions, uses BT.2020 conversion
coefficients, and requires PQ/BT.2020 output metadata. Neither this test nor the startup probe
permits software decoding. GPU readback is used only by the opt-in precision test.

The macOS `profile_probe.rs` HDR444 parameter-set constants and length-prefixed IDR use the
HDR444 black probe command above with `size=64x64:rate=1`. Each Annex-B NAL is extracted without
its start code; the IDR is prefixed by its four-byte big-endian length. The decoded frame must
have two full-sized IOSurface planes and pass the existing PQ/BT.2020 Metal import checks.

## Main10 PQ hardware precision regression

`hevc-p010-pq-precision.hevc` was generated with FFmpeg 9.0.1 and libx265. It is Main10
4:2:0, not lossless RExt: QP0 is used with psychovisual processing, SAO and deblocking disabled.
The source repeats Y codes64–940 every877 pixels, with neutral Cb/Cr=512. Quantization produces
875 distinct decoded gray levels and at most two source-code values of codec error. The binary
`hevc-p010-pq-precision-luma.bin` contains the first877 software-decoded luma values as little-endian
u16, so the hardware test compares against the actual coded signal and keeps the independent
RGB conversion tolerance at two codes.

```sh
ffmpeg -hide_banner -loglevel error -f lavfi \
    -i "nullsrc=size=1920x1080:rate=60,format=yuv420p10le,geq=lum='64+mod(X,877)':cb=512:cr=512" \
    -frames:v 1 -c:v libx265 -preset ultrafast \
    -x265-params 'log-level=error:pools=1:frame-threads=1:info=0:keyint=60:bframes=0:repeat-headers=1:qp=0:psy-rd=0:psy-rdoq=0:sao=0:deblock=0:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:chromaloc=0' \
    -f hevc -y hevc-p010-pq-precision.hevc
ffprobe -v error -show_entries stream=profile,pix_fmt,color_space,color_transfer,color_primaries \
    -of compact hevc-p010-pq-precision.hevc
ffmpeg -v error -i hevc-p010-pq-precision.hevc -frames:v 1 -pix_fmt yuv420p10le \
    -f rawvideo -y pq-decoded.yuv
python -c "from pathlib import Path; Path('hevc-p010-pq-precision-luma.bin').write_bytes(Path('pq-decoded.yuv').read_bytes()[:877*2])"
```

## 5K HDR resolution regression

`hevc-p010-5k-pq.hevc` is a synthetic single-frame 5120x2880, ten-bit 4:2:0
HEVC image with PQ/BT.2020 metadata. The explicit hardware regression checks
negotiated dimensions, native P010 fallback from a Y410 request, D3D11 RGB10A2
conversion and recovery without stale frames. It is not a startup capability
probe or a performance benchmark. Generated locally with:

```sh
ffmpeg -hide_banner -loglevel error -f lavfi \
  -i 'testsrc2=size=5120x2880:rate=1' -frames:v 1 \
  -pix_fmt yuv420p10le -c:v libx265 -preset ultrafast \
  -x265-params 'log-level=error:pools=4:frame-threads=1:info=0:keyint=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc' \
  -f hevc -y hevc-p010-5k-pq.hevc
```

SHA256: `df7e79111e51d92e797143376f2990a4b69de1effd5932a709ee2625ecb31ddf`.

## 5K HDR 4:4:4 zero-copy D3D11VA regression

`hevc-y410-5k-pq.hevc` is a synthetic 300-frame (2.5 s at 120 fps)
5120x2880 Main44410 (RExt) HEVC stream with PQ/BT.2020 metadata. Every 10-bit
code value in every plane is the flat constant 876 (`0x036C`, encoded
losslessly), so the D3D11VA acceptance test asserts exact DXGI Y410 samples
after decoding. Raw intermediates are generated outside the repository under
`..\tmp-fixtures\` (fixture hygiene); only this small encoded stream lives in
the fixture directory. Generated on the dev machine with libx265:

```sh
# flat10.yuv: one yuv444p10le frame whose every 16-bit code is 0x036C, under ..\tmp-fixtures\, then:
ffmpeg -hide_banner -loglevel warning -stream_loop 299 \
  -f rawvideo -pix_fmt yuv444p10le -s 5120x2880 -r 120 -i ..\tmp-fixtures\flat10.yuv \
  -frames:v 300 -c:v libx265 -preset ultrafast \
  -x265-params 'log-level=error:pools=4:frame-threads=1:keyint=300:min-keyint=300:scenecut=0:lossless=1:repeat-headers=1:colorprim=bt2020:transfer=smpte2084:colormatrix=bt2020nc:chromaloc=0' \
  -color_primaries bt2020 -color_trc smpte2084 -colorspace bt2020nc -color_range tv \
  -f hevc -y hevc-y410-5k-pq.hevc
```

SHA256: `a117e1079704f6c72aab344de0b712f03338a904d1b595d1bfbd8e9737fc8082`.

Decoding this stream through D3D11VA requires the patched FFmpeg SDK (stock
FFmpeg never offers the D3D11 hardware format for 4:4:4 HEVC): see
`native/opennow-streamer/vendor/patches/ffmpeg-d3d11va-hevc-444.patch`.

## 5K HDR 4:4:4 motion clips (reference-integrity regression)

`hevc-y410-5k-motion-bf0.hevc` (782,042 bytes) and
`hevc-y410-5k-motion-bf2.hevc` (513,094 bytes) are real-motion 5120x2880
10-bit 4:4:4 RExt clips (60 frames each, 0.5 s at 120 fps, `testsrc2`
source, `-g 120`, `-refs 16`, `bf 0` and `bf 2`) encoded with the NVENC
build in `deps/ffmpeg-n9.0-latest-win64-lgpl-shared-9.0`. Unlike the flat fixtures
they exercise P/B inter-prediction and several references, which is what
the live block-smear regression needed. The offline PSNR test decodes each
clip through D3D11VA and through FFmpeg software and requires >= 50 dB on
every frame; encode quality is irrelevant to that metric (both sides decode
the same bits). Raw intermediates stay in `..\tmp-fixtures\`.

```sh
ffmpeg -f lavfi -i "testsrc2=size=5120x2880:rate=120,format=yuv444p10le" \
  -t 0.5 -c:v hevc_nvenc -profile:v rext -pix_fmt yuv444p10le -g 120 \
  -refs 16 -bf 0 hevc-y410-5k-motion-bf0.hevc
# and -bf 2 for hevc-y410-5k-motion-bf2.hevc
```

SHA256: bf0 `6ff91a760b0aefdba14197e31909c6b706fd24f9435d8b3ff781f379a95a288a`, bf2 `316452e80043ccd50248729997ebc11931176c49cbdebb3e0aa5b149e9af607d`.
