def compute(assign: bool):
    if assign:
        value = 0
    try:
        print(value)
    except UnboundLocalError as error:
        print(error)
    try:
        value = 42
        raise ValueError("keep the local")
    except ValueError:
        print(value)


compute(False)
compute(True)
