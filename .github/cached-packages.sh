#!/usr/bin/env bash
# Apt packages restored from the cache (awalsh128/cache-apt-pkgs-action),
# made to work. The cache restores the packages' files, not what their
# install scripts make: the BLAS and LAPACK links (behind alternatives)
# that ffmpeg and numpy load. Those are made here; if CHECK still fails,
# the packages are installed the slow way.
#   bash .github/cached-packages.sh CHECK PACKAGE...
set -euo pipefail
check="$1"
shift
dir=/usr/lib/x86_64-linux-gnu
for lib in blas lapack; do
  if [ ! -e "$dir/lib$lib.so.3" ] && [ -e "$dir/$lib/lib$lib.so.3" ]; then sudo ln -s "$dir/$lib/lib$lib.so.3" "$dir/lib$lib.so.3"; fi
done
sudo ldconfig
if ! bash -c "$check" > /dev/null; then
  echo "::warning::the cached packages do not work as restored; installing them"
  sudo apt-get update -q
  sudo apt-get install -y -q "$@"
fi
