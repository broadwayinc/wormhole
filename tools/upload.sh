#!/bin/sh
# Uploads the release in dist/ (made by tools/package.sh) to the bucket behind cdn.broadwayinc.com:
#   dist/get/wormhole/...  ->  s3://broadwayinc-drive/get/wormhole/...
# After it, `curl -fsSL https://cdn.broadwayinc.com/get/wormhole/v1 | sh` installs this release.
#
#   sh tools/upload.sh              upload the version named in dist/get/wormhole/latest
#   sh tools/upload.sh 0.1.0        upload that version (latest and v1 are uploaded only for the newest)
#   sh tools/upload.sh --dry-run    show what would be uploaded, upload nothing
#
# Uses the AWS CLI with the credentials of the current shell. After uploading `v1` and `latest` it clears
# their copies on the CDN (CloudFront distribution E2T766Y04CBXZ1; set CLOUDFRONT_DISTRIBUTION_ID to use
# another). The version files never change, so they need no clearing.
# Upload order: the version files first, then `latest`, then the installer, so `latest` never names
# files that are not there yet. Same as DDBD's tools/upload.sh.

set -eu
cd "$(dirname "$0")/.."

bucket="broadwayinc-drive"
distribution="${CLOUDFRONT_DISTRIBUTION_ID:-E2T766Y04CBXZ1}"
src="dist/get/wormhole"
dst="s3://$bucket/get/wormhole"
dry=""
version=""
for a in "$@"; do
    case "$a" in
        --dry-run) dry="--dryrun" ;;
        [0-9]*.[0-9]*.[0-9]*) version="$a" ;;
        *) echo "Unknown argument: $a (use a version such as 0.1.0, or --dry-run)" >&2; exit 1 ;;
    esac
done

command -v aws >/dev/null 2>&1 || { echo "The AWS CLI (aws) is needed." >&2; exit 1; }
[ -f "$src/latest" ] && [ -f "$src/v1" ] || { echo "No release in $src. Make one first: sh tools/package.sh" >&2; exit 1; }
newest="$(tr -d ' \r\n' < "$src/latest")"
version="${version:-$newest}"
dir="$src/$version"
[ -d "$dir" ] || { echo "No files for version $version in $dir." >&2; exit 1; }

# every archive must match its checksum file, or a damaged pair would be published
found=0
for f in "$dir"/*.tar.gz; do
    [ -f "$f" ] || continue
    found=1
    (cd "$dir" && sha256sum -c --quiet "$(basename "$f").sha256") || { echo "$f does not match its .sha256 file; run tools/package.sh again." >&2; exit 1; }
done
[ "$found" = 1 ] || { echo "No .tar.gz files in $dir." >&2; exit 1; }

immutable="public, max-age=31536000, immutable"
echo "Uploading wormhole $version to $dst/ ${dry:+(dry run)}"
for f in "$dir"/*.tar.gz; do
    aws s3 cp $dry --no-progress "$f" "$dst/$version/" --content-type application/gzip --cache-control "$immutable"
    aws s3 cp $dry --no-progress "$f.sha256" "$dst/$version/" --content-type "text/plain; charset=utf-8" --cache-control "$immutable"
done

if [ "$version" = "$newest" ]; then
    aws s3 cp $dry --no-progress "$src/latest" "$dst/latest" --content-type "text/plain; charset=utf-8" --cache-control "no-cache"
    aws s3 cp $dry --no-progress "$src/v1" "$dst/v1" --content-type "text/plain; charset=utf-8" --cache-control "no-cache"
    if [ -n "$dry" ]; then
        echo "(dryrun) clear the CDN copies of /get/wormhole/v1 and /get/wormhole/latest (distribution $distribution)"
    else
        id="$(aws cloudfront create-invalidation --distribution-id "$distribution" \
            --paths /get/wormhole/v1 /get/wormhole/latest --query 'Invalidation.Id' --output text)"
        echo "Clearing the CDN copies of v1 and latest (distribution $distribution, invalidation $id); this takes a minute or two."
    fi
else
    echo "$version is not the newest version ($newest): latest and v1 were left as they are."
fi

[ -n "$dry" ] && { echo "Dry run: nothing was uploaded."; exit 0; }
echo ""
echo "Done. Check it from any Linux machine:"
echo "  curl -fsSL https://cdn.broadwayinc.com/get/wormhole/latest"
echo "  curl -fsSL https://cdn.broadwayinc.com/get/wormhole/v1 | sh"
