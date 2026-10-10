import numpy as np
from numba import njit


@njit(cache=True)
def sum_squares(values):
    return np.sum(values * values)
