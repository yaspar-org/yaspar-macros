#!/usr/bin/env bash

# Amazon's SPDX header, which every `.rs` file must carry in its first two lines:
#
#   // Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.
#   // SPDX-License-Identifier: Apache-2.0
copyright='Copyright Amazon.com, Inc. or its affiliates. All Rights Reserved.'
license='SPDX-License-Identifier: Apache-2.0'

# Defaults to the whole repository, so a file added in a new directory is covered
# without touching this script or the workflow that calls it.
folder=${1:-$(dirname "$(dirname "$(realpath "$0")")")}

error=0
checked=0

while IFS= read -r -d '' f
do
  checked=$((checked + 1))
  head=$(head -n2 "$f")
  if ! grep -q -F "$copyright" <<< "$head"; then
    error=1
    echo "$f does not have a copyright banner!"
  elif ! grep -q -F "$license" <<< "$head"; then
    error=1
    echo "$f does not have an SPDX license identifier!"
  fi
done <   <(find "$folder" \( -name target -o -name .git \) -type d -prune -o -name '*.rs' -print0)

if [[ $error == 1 ]]; then
  echo "Both lines of the SPDX header must be attached in the first two lines in every source code file!"
  exit 1
fi

if [[ $checked == 0 ]]; then
  echo "No .rs files found under $folder -- is this the right directory?"
  exit 1
fi

echo "SPDX header present in all $checked .rs files under $folder"
exit 0
