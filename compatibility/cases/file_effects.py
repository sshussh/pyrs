with open("output.txt", "w") as stream:
    stream.write("sample,value\na,1\nb,2\n")
with open("output.txt", "r") as stream:
    print(stream.read(), end="")
