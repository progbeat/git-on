import numpy as np
from local_math import sum_squares

assert sum_squares(np.arange(10, dtype=np.float64)) == 285.0
print("sum-squares=285")
