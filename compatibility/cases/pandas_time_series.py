import pandas as pd

dates = pd.date_range("2026-01-01", periods=6, freq="h", tz="UTC")
values = pd.Series([1, 2, 3, 4, 5, 6], index=dates)
print(values.resample("3h").sum().tolist())
print(values.rolling(3, min_periods=1).mean().tolist())
print(dates.tz_convert("Asia/Amman").hour.tolist())
