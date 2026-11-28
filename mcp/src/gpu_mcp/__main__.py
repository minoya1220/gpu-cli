"""Entry point: gpu-mcp [--config PATH]."""

from __future__ import annotations

import argparse
import sys

from .config import ConfigError, load


def main(argv: list[str] | None = None) -> int:
    parser = argparse.ArgumentParser(prog="gpu-mcp", description=__doc__)
    parser.add_argument("--config", help="TOML config (default: $GPU_MCP_CONFIG or ~/.config/gpu-mcp/config.toml)")
    parser.add_argument("--check", action="store_true", help="Validate the config and exit")
    args = parser.parse_args(argv)
    try:
        config = load(args.config)
    except ConfigError as exc:
        print(f"gpu-mcp: {exc}", file=sys.stderr)
        return 2
    if args.check:
        print(f"gpu-mcp: config ok; serving {config.resource_url} from http://{config.host}:{config.port}")
        return 0
    import uvicorn

    from .server import build

    app = build(config)
    uvicorn.run(app, host=config.host, port=config.port, proxy_headers=False, log_level="info")
    return 0


if __name__ == "__main__":
    sys.exit(main())
