#!/bin/sh
# Regenerates the tiny test files: 0.2 s of silence with standard tags, one custom field
# and an embedded front cover. Needs ffmpeg and python-mutagen.
set -e
cd "$(dirname "$0")"
ffmpeg -v error -y -f lavfi -i color=c=red:s=16x16 -frames:v 1 cover.jpg
ffmpeg -v error -y -f lavfi -i color=c=blue:s=16x16 -frames:v 1 other.jpg
for ext in flac mp3 m4a; do
  ffmpeg -v error -y -f lavfi -i anullsrc=r=44100:cl=mono -t 0.2 -map_metadata -1 "tiny.$ext"
done
python3 - <<'PY'
from mutagen.flac import FLAC, Picture
from mutagen.id3 import ID3, APIC, TXXX, TIT2, TPE1, TPE2, TALB, TRCK, TCON
from mutagen.mp4 import MP4, MP4Cover, MP4FreeForm

cover = open("cover.jpg", "rb").read()
std = dict(title="Song", artist="Artist", albumartist="Album Artist",
           album="Album", tracknumber="3", genre="Rock")

f = FLAC("tiny.flac")
for k, v in std.items():
    f[k] = v
f["tidal_track_id"] = "42"
pic = Picture()
pic.type, pic.mime, pic.data = 3, "image/jpeg", cover
f.add_picture(pic)
f.save()

t = ID3()
t.add(TIT2(text="Song")); t.add(TPE1(text="Artist")); t.add(TPE2(text="Album Artist"))
t.add(TALB(text="Album")); t.add(TRCK(text="3")); t.add(TCON(text="Rock"))
t.add(TXXX(desc="TIDAL_TRACK_ID", text="42"))
t.add(APIC(mime="image/jpeg", type=3, desc="", data=cover))
t.save("tiny.mp3", v2_version=4)

m = MP4("tiny.m4a")
m["\xa9nam"], m["\xa9ART"], m["aART"] = "Song", "Artist", "Album Artist"
m["\xa9alb"], m["trkn"], m["\xa9gen"] = "Album", [(3, 0)], "Rock"
m["----:com.apple.iTunes:TIDAL_TRACK_ID"] = [MP4FreeForm(b"42")]
m["covr"] = [MP4Cover(cover, MP4Cover.FORMAT_JPEG)]
m.save()
PY
cp tiny.mp3 tiny23.mp3
python3 -c "from mutagen.id3 import ID3; t = ID3('tiny23.mp3'); t.update_to_v23(); t.save(v2_version=3)"
