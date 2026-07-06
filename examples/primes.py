# Print the primes below 50 using trial division
def is_prime(n: int) -> bool:
    if n < 2:
        return False
    d = 2
    while d * d <= n:
        if n % d == 0:
            return False
        d += 1
    return True

n = 2
while n < 50:
    if is_prime(n):
        print(n)
    n += 1
