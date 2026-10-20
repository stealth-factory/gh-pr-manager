// gh-pr-manager - a terminal UI for GitHub pull requests
// Copyright (C) 2026 William Li
// SPDX-License-Identifier: AGPL-3.0-or-later

'use strict';

// Optional dependencies that do not match `os` / `cpu` / `libc` are
// skipped, and a failed optional fetch is only a warning. Either way npm
// reports success and leaves a package that cannot run. Failing here turns
// that into a sentence at install time, while somebody is still looking.

const platform = require('./platform');

try {
  platform.requireInstalled();
} catch (err) {
  console.error(err.message);
  process.exit(1);
}
