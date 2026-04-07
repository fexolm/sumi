/* Pulls libm via -lm-less explicit symbols (gcc auto-handles). Verifies that
   double arithmetic and IEEE rounding work end-to-end inside sumi. */
#include <math.h>
#include <stdio.h>

int main(void) {
    double a = sqrt(2.0);
    /* sqrt(2) ≈ 1.4142135 */
    if (a < 1.41 || a > 1.42) return 1;

    double b = sin(0.0);
    if (b < -1e-9 || b > 1e-9) return 2;

    double c = cos(0.0);
    if (c < 0.9999 || c > 1.0001) return 3;

    /* fabs */
    if (fabs(-3.5) != 3.5) return 4;

    /* pow */
    double d = pow(2.0, 10.0);
    if (d < 1023.5 || d > 1024.5) return 5;

    printf("math ok: sqrt(2)=%.4f sin(0)=%.4f cos(0)=%.4f pow(2,10)=%.0f\n", a, b, c, d);
    return 0;
}
