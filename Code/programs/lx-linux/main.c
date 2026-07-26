/* main.c — харнесс (Веха 55): наша программа VOID зовёт НЕИЗМЕНЁННЫЙ lib/sort.c ядра Linux.
 *
 * sort.c (linux-src/, verbatim Linux 6.18.7) собран против рукописных шим-заголовков linux/*.h и
 * линкуется в этот же ELF. Здесь мы даём ему массив и колбэк сравнения — и проверяем, что реальный
 * heapsort ядра Linux отсортировал его на VOID. Первый реальный .c из Linux, работающий на VOID.
 */
#include <stdio.h>

#include <linux/sort.h> /* sort() — из настоящего ядра Linux */

/* Колбэк сравнения int. Внутри — макрос cmp_int() из реального <linux/sort.h> (трёхстороннее
 * сравнение ядра); имя функции иное, чтобы не столкнуться с этим макросом. */
static int int_cmp(const void *a, const void *b)
{
    int x = *(const int *)a, y = *(const int *)b;
    return cmp_int(x, y);
}

int main(void)
{
    int arr[] = { 42, 7, 99, 1, 815, 4, 7, 0, 271, 100, 3, 2, 88, 5, 13 };
    size_t n = sizeof(arr) / sizeof(arr[0]);

    sort(arr, n, sizeof(int), int_cmp, NULL); /* ← вызов реального кода ядра Linux */

    int ok = 1;
    for (size_t i = 1; i < n; i++)
        if (arr[i - 1] > arr[i])
            ok = 0;

    printf("[lx-sort] Linux lib/sort.c (6.18.7, неизменённый) на VOID: ");
    for (size_t i = 0; i < n; i++)
        printf("%d ", arr[i]);
    printf("\n[lx-sort] порядок %s -- код ядра Linux РАБОТАЕТ на VOID\n",
           ok ? "верен" : "НЕВЕРЕН");
    return ok ? 0 : 1;
}
