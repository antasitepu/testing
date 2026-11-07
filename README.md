# Gold Monitoring

Analysis plans and the backtest engine for XAUUSDT, published so backtests can
run on GitHub Actions.

This repository is a one-way snapshot of a private repository. It holds only
the Rust workspace, its migrations and the backtest workflow. Pull requests are
not merged here.

The market dataset is not stored in this repository. The `backtest` workflow
downloads it with a repository secret and runs only on manual dispatch.

## License

Copyright (c) 2026 antasitepu. All rights reserved.

The source is public only so it can be read and run by this repository's CI.
No license is granted to use, copy, modify or distribute it.
