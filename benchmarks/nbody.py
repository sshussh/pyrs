# Mixed float + list workload: softened 5-body gravity, 40k steps
# (force uses 1/r^2 with softening and no sqrt so both runtimes
# produce bit-identical IEEE results)
xs = [0.0, 1.0, -1.0, 0.5, -0.5]
ys = [0.0, 0.5, -0.5, -1.0, 1.0]
vxs = [0.0, 0.1, -0.1, 0.05, -0.05]
vys = [0.05, -0.05, 0.1, -0.1, 0.0]
ms = [1.0, 0.9, 0.8, 0.7, 0.6]

n = 5
dt = 0.001
for step in range(100000):
    for i in range(n):
        ax = 0.0
        ay = 0.0
        for j in range(n):
            if i != j:
                dx = xs[j] - xs[i]
                dy = ys[j] - ys[i]
                r2 = dx * dx + dy * dy + 0.01
                f = ms[j] / (r2 * r2)
                ax += f * dx
                ay += f * dy
        vxs[i] += ax * dt
        vys[i] += ay * dt
    for i in range(n):
        xs[i] += vxs[i] * dt
        ys[i] += vys[i] * dt

checksum = 0.0
for i in range(n):
    checksum += xs[i] * xs[i] + ys[i] * ys[i]
print(checksum)
