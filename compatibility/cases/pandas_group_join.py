import pandas as pd

df = pd.DataFrame({"team": ["a", "b", "a", "b"], "value": [1.0, 2.0, 3.0, None]})
totals = df.groupby("team", as_index=False)["value"].sum()
labels = pd.DataFrame({"team": ["a", "b"], "label": ["alpha", "beta"]})
print(totals.merge(labels, on="team", validate="one_to_one").to_json(orient="records"))
print(df["value"].fillna(0).tolist())
print(df.loc[df["value"] > 1, "team"].tolist())
