# String ops: character iteration and comparisons over 100k chars, 20 passes
text = "the quick brown fox jumps over the lazy dog " * 2000
vowels = 0
for p in range(60):
    for c in text:
        if c == "a" or c == "e" or c == "i" or c == "o" or c == "u":
            vowels += 1
print(len(text), vowels)
