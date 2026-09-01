"""Command-line interface, mainly for scripted use and testing.

  python -m unflash.cli analyze VIDEO [--start S] [--duration D] [--profile P]
  python -m unflash.cli scan VIDEO [--profile P]

Profiles: wcag_ext (default), wcag, strict.
"""

import argparse
import json
import sys

from .config import DEFAULT_PROFILE, PROFILES, profile_config
from .analysis import analyze_file, violations_to_sections, timeline_summary
from . import ffio


def main(argv=None):
    ap = argparse.ArgumentParser(prog="unflash")
    sub = ap.add_subparsers(dest="cmd", required=True)
    for name in ("analyze", "scan"):
        p = sub.add_parser(name)
        p.add_argument("video")
        p.add_argument("--start", type=float, default=None)
        p.add_argument("--duration", type=float, default=None)
        p.add_argument("--profile", choices=sorted(PROFILES),
                       default=DEFAULT_PROFILE,
                       help="detection profile (default: %(default)s)")
        p.add_argument("--wcag", action="store_true",
                       help="shorthand for --profile wcag "
                            "(exact WCAG, extended flashes ignored)")
        p.add_argument("--json", action="store_true")
    args = ap.parse_args(argv)

    cfg = profile_config("wcag" if args.wcag else args.profile)
    info = ffio.probe(args.video)

    def progress(p):
        sys.stderr.write(f"\r{p * 100:5.1f}% ")
        sys.stderr.flush()

    res = analyze_file(args.video, cfg, start=args.start,
                       duration=args.duration, progress=progress, info=info)
    sys.stderr.write("\r        \r")

    if args.cmd == "analyze":
        out = res.to_dict()
        if args.json:
            print(json.dumps(out, indent=2))
        else:
            print(f"frames={out['frames']} duration={out['duration']:.2f}s "
                  f"safe={out['safe']}")
            for v in out["violations"]:
                # onset is where the flashing that adds up to the failure
                # starts; peak is the worst moment to go and look at
                print(f"  {v['kind']:>9}  {v['onset']:8.3f} - {v['end']:8.3f}"
                      f"  (worst {v['peak']:.3f}, count {v['count']:.0f})")
            if not out["violations"]:
                print("  no violations")
    else:
        idx = ffio.index_video(args.video)
        bounds = (idx["ts_min"], idx["ts_max"])
        sections = violations_to_sections(res.violations, cfg, bounds, None)
        timeline = timeline_summary(res, bounds)
        if args.json:
            print(json.dumps({"sections": sections, "timeline": timeline,
                              "safe": res.safe}, indent=2))
        else:
            print(f"safe={res.safe}  sections={len(sections)}")
            for s in sections:
                print(f"  {s['start']:8.3f} - {s['end']:8.3f}  "
                      f"[{','.join(s['kinds'])}]")
    return 0


if __name__ == "__main__":
    sys.exit(main())
