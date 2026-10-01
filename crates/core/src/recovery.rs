//! Instructions and a script embedded in every pack, so data can be recovered
//! with only `par2`, `tar`, and `zstd`, without this program or its catalog.

pub const RECOVERY_TXT: &str = r#"HOW TO RECOVER FILES FROM THIS PACK WITHOUT THE ARCHIVE APP
===========================================================

This pack is a normal, uncompressed tar file. It holds files from one
project (a folder that was sent to the archive); .archive/PACK.json names the
project and where it was in the archive. Each original file is stored inside
it as a zstd-compressed file named "<original name>.zst". Many small files are
grouped into compressed tar files under ".archive/solid-*.tar.zst". You need
three standard, free tools: par2 (par2cmdline), tar, and zstd.

1. Check and repair the pack (the .par2 files sit next to the pack):

       par2 verify PACK.tar.par2
       par2 repair PACK.tar.par2        # only if verify reports damage

2. Extract everything into a folder:

       sh recover.sh /path/to/PACK.tar /path/to/output

   The script is inside the pack at .archive/recover.sh. To get it out:

       tar -xf PACK.tar .archive/recover.sh

   Or do it by hand, in an empty folder:

       mkdir stage && tar -xf PACK.tar -C stage && cd stage
       decompress each member listed by "tar -tf PACK.tar" once, into the
       output folder, overwriting any earlier copy:
           zstd -d -f "path/name.zst" -o "/path/to/output/path/name"
       then unpack the small-file blocks there:
           for b in .archive/solid-*.tar.zst; do zstd -dc "$b" | (cd /path/to/output && tar -xf -); done

3. A project often spans several packs. Recover all of its packs, lowest
   number first, into the same output folder to rebuild the whole project.

FINDING ONE FILE

INDEX.tsv, at the top of the archive folder, lists every file (updated
daily): its path in the archive, size, SHA-256, the pack holding it, and
where in the pack its bytes are. To get one file out:

    grep "some name" INDEX.tsv

then, with that line's PACK, START, LENGTH, OFFSET, and SIZE:

    tail -c +$((START + 1)) packs/PACK | head -c LENGTH | zstd -dc | tail -c +$((OFFSET + 1)) | head -c SIZE > FILE

(A SIZE of 0 is an empty file: just create it. The MEMBER and STORED_AS columns name the same bytes for tar: for a small
file, "tar -xOf packs/PACK MEMBER | zstd -dc | tar -xOf - STORED_AS", and
otherwise "tar -xOf packs/PACK MEMBER | zstd -dc". STORED_AS differs from
the path if the project was renamed or moved since. macOS's tar may not
match names with accents this way; the command above always works.)

.archive/MANIFEST.json lists every file in this pack with its SHA-256 checksum,
size, and modification time. Identical files in a project were stored only
once. The other copies are listed in .archive/DUPLICATES.tsv as "copy <TAB>
stored copy <TAB> sha256 <TAB> modification time" (the time in touch -t form,
UTC); recover.sh recreates them by copying the stored file, which may come from
an earlier pack of the same project (so recover packs in order into the same
folder), then gives each copy its own modification time.

Verify a recovered file with:   shasum -a 256 FILE   (or sha256sum FILE)
"#;

pub const RECOVER_SH: &str = r#"#!/bin/sh
# Recover every file from one archive pack using only tar and zstd.
# Usage: sh recover.sh /path/to/PACK.tar /path/to/output
# Recover a project's packs in order (lowest number first) into the same
# output folder; a file stored again in a later pack replaces the earlier copy.
set -eu
if [ $# -ne 2 ]; then
  echo "usage: sh recover.sh PACK.tar OUTPUT_DIR" >&2
  exit 2
fi
case "$1" in
  /*) pack="$1" ;;
  *) pack="$(pwd)/$1" ;;
esac
mkdir -p "$2"
out="$(cd "$2" && pwd)"

# Unpack into a staging folder first, so compressed members never collide
# with recovered files (an original named "x.zst" next to one named "x").
stage="$out/.recover-stage.$$"
rm -rf "$stage"
mkdir "$stage"
trap 'rm -rf "$stage"' EXIT
tar -xf "$pack" -C "$stage"
cd "$stage"

# Every large file was stored as NAME.zst (small ones are in the blocks
# below). Decompress each into the output folder, replacing an earlier copy.
find . -type f -name '*.zst' ! -path './.archive/*' -exec sh -c '
  set -e
  out="$1"; shift
  for m do
    mkdir -p "$out/$(dirname -- "$m")"
    zstd -dqf -o "$out/${m%.zst}" -- "$m"
    rm -f -- "$m"
  done' sh "$out" {} +

# Small files are grouped into compressed tar blocks.
for b in .archive/solid-*.tar.zst; do
  [ -e "$b" ] || continue
  zstd -dcq -- "$b" | (cd "$out" && tar -xf -)
  rm -f -- "$b"
done

# What's left: links, empty folders, and this pack's notes (.archive/).
tar -cf - . | (cd "$out" && tar -xf -)
cd "$out"

# Identical files were stored once; recreate the other copies.
if [ -f .archive/DUPLICATES.tsv ]; then
  tab="$(printf '\t')"
  while IFS="$tab" read -r copy stored sha when; do
    if [ -f "$stored" ]; then
      mkdir -p "$(dirname -- "$copy")"
      cp -pf -- "$stored" "$copy"
      # The copy's own modification time (packs sealed before this have none).
      if [ -n "${when:-}" ]; then
        TZ=UTC0 touch -m -t "$when" -- "$copy"
      fi
    else
      echo "note: $copy is a copy of $stored, which is in an earlier pack; recover that pack first" >&2
    fi
  done < .archive/DUPLICATES.tsv
fi
echo "Recovered into $out"
echo "See .archive/MANIFEST.json for checksums of every file."
"#;
