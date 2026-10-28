# Build ib-imessage's committed background templates from the captures,
# with UUIDs replaced by fixed placeholders and the photo replaced by a
# neutral generated image. Zip order and methods are kept.
import io, os, re, sys, zipfile, plistlib, subprocess, json, tempfile, shutil
from PIL import Image

BG = sys.argv[1]
OUT = sys.argv[2]
PREP = sys.argv[3]
os.makedirs(OUT, exist_ok=True)
U = re.compile(rb'[0-9A-F]{8}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{4}-[0-9A-F]{12}')


def placeholders(zf):
    seen = []
    for i in zf.infolist():
        blobs = [i.filename.encode()]
        if not i.filename.endswith('.HEIC'):
            blobs.append(zf.read(i.filename))
        for b in blobs:
            for u in U.findall(b):
                if u not in seen:
                    seen.append(u)
    return {u: ('00000000-0000-4000-8000-%012d' % (n + 1)).encode() for n, u in enumerate(seen)}


def sub(b, m):
    for k, v in m.items():
        b = b.replace(k, v)
    return b


def rezip(entries):
    buf = io.BytesIO()
    with zipfile.ZipFile(buf, 'w') as z:
        for info, data in entries:
            ni = zipfile.ZipInfo(info.filename, date_time=(2026, 1, 1, 0, 0, 0))
            ni.compress_type = info.compress_type
            ni.external_attr = info.external_attr
            ni.create_system = 3
            z.writestr(ni, data)
    return buf.getvalue()


def poster_entries(outer):
    return zipfile.ZipFile(io.BytesIO(outer.read('transcriptBackground/poster')))


# --- photo template
outer = zipfile.ZipFile(os.path.join(BG, 'captured-background.zip'))
inner = poster_entries(outer)
m = placeholders(inner)
print('photo uuids', {k.decode(): v.decode() for k, v in m.items()})
tmp = tempfile.mkdtemp()
neutral = os.path.join(tmp, 'neutral.png')
g = Image.new('RGB', (1292, 2796))
px = g.load()
for y in range(2796):
    t = y / 2795
    c = (int(90 + 60 * t), int(110 + 50 * t), int(140 + 40 * t))
    for x in range(1292):
        px[x, y] = c
g.save(neutral)
res = json.loads(subprocess.check_output([sys.executable, '-I', PREP, neutral, tmp]))
print('neutral derived', res)
heic = open(os.path.join(tmp, 'layer.heic'), 'rb').read()
watchpng = open(os.path.join(tmp, 'watch.png'), 'rb').read()
ents = []
for i in inner.infolist():
    d = heic if i.filename.endswith('.HEIC') else sub(inner.read(i.filename), m)
    i2 = zipfile.ZipInfo(sub(i.filename.encode(), m).decode())
    i2.compress_type, i2.external_attr = i.compress_type, i.external_attr
    ents.append((i2, d))
open(os.path.join(OUT, 'photo-poster.zip'), 'wb').write(rezip(ents))
w = plistlib.loads(outer.read('transcriptBackground/watchBackground'))
w['backgroundImageData'] = watchpng
w['luminance'] = res['luminance']
open(os.path.join(OUT, 'photo-watch.plist'), 'wb').write(plistlib.dumps(w, fmt=plistlib.FMT_BINARY, sort_keys=False))

# --- gradient/dynamic template (from the gradient capture)
outer = zipfile.ZipFile(os.path.join(BG, 'bg-3F85EC93-DF04-4D76-BA98-C15858A451CB.zip'))
inner = poster_entries(outer)
m = placeholders(inner)
print('simple uuids', {k.decode(): v.decode() for k, v in m.items()})
ents = []
for i in inner.infolist():
    i2 = zipfile.ZipInfo(sub(i.filename.encode(), m).decode())
    i2.compress_type, i2.external_attr = i.compress_type, i.external_attr
    ents.append((i2, sub(inner.read(i.filename), m)))
open(os.path.join(OUT, 'simple-poster.zip'), 'wb').write(rezip(ents))

# --- dynamic previews (Apple renders of the presets)
for gid in ['3428F69E-3049-4987-BA52-DD023D3139DE', '6C8E26EA-A192-46FA-AE86-7CB42722D04D', 'B57FDEF2-8377-4EF9-BCE5-62A490A5FF2B']:
    z = zipfile.ZipFile(os.path.join(BG, 'bg-%s.zip' % gid))
    preset = plistlib.loads(poster_entries(z).read('configuration/versions/0/contents/com.apple.posterkit.provider.contents.userInfo'))['identifier']
    wb = plistlib.loads(z.read('transcriptBackground/watchBackground'))
    open(os.path.join(OUT, 'dynamic-%s.png' % preset), 'wb').write(wb['backgroundImageData'])
    print('preset', preset, len(wb['backgroundImageData']))
shutil.rmtree(tmp)
