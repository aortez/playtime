# Computation tools

Save scripts in `/work`, then execute them with `python script.py`.

For plotting without a display:

```python
import matplotlib
matplotlib.use("Agg")
import matplotlib.pyplot as plt
plt.plot([0, 1, 2], [0, 1, 4])
plt.savefig("/work/plot.png")
```

NumPy: `import numpy as np`. For reproducible random samples, create a generator
with an explicit seed: `rng = np.random.default_rng(42)`.

SciPy: `from scipy import signal, integrate, optimize, stats`.
Pillow: `from PIL import Image, ImageDraw`.

Record the parameters and commands needed to reproduce your own experiments.
