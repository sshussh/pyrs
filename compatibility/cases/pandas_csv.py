import pandas as pd
from io import StringIO

df = pd.read_csv(StringIO("name,value\nalpha,1.5\nbeta,2.5\n"))
df["double"] = df["value"] * 2
df.to_csv("result.csv", index=False, lineterminator="\n")
print(pd.read_csv("result.csv").to_json(orient="records"))
