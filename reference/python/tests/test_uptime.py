import pytest

from gpu.cli import render_instances
from gpu.instances import format_uptime, instance_label


@pytest.mark.parametrize(("value", "expected"), [
    (0, "0s"), (0.9, "0s"), (45, "45s"), (59.99, "59s"),
    (60, "1m"), (720, "12m"), (3599.99, "59m"),
    (3600, "1h 00m"), (7342.81, "2h 02m"), ("7342.81", "2h 02m"),
    (86399, "23h 59m"), (86400, "1d 00h"), (273600, "3d 04h"),
    (None, "-"), ("", "-"), ("garbage", "-"), ("2h", "-"),
    (-1, "-"), (float("nan"), "-"), (float("inf"), "-"),
    ("-inf", "-"), (True, "-"), ({}, "-"),
])
def test_format_uptime(value, expected):
    assert format_uptime(value) == expected


@pytest.mark.parametrize("duration", [None, "garbage", 0, "7342.81"])
def test_label_and_table_share_uptime(duration, capsys):
    item = {"id": 123, "duration": duration}
    expected = format_uptime(duration)
    assert instance_label(item) == f"123  -  -  -  {expected}  -"
    render_instances([item])
    assert capsys.readouterr().out.splitlines()[-1].rstrip().endswith(expected)
