/* main_driver.c — харнесс driver-model + module_init Lx_kit (Веха 66).
 *
 * Фейковые шина/драйвер/устройство проходят весь жизненный цикл ядра: module_init перенаправлен
 * в lx_module_init → driver_register → match по шине → probe; drvdata; device_unregister → remove.
 * Проверяет, что связка register/match/probe/remove работает — на ней встанет PCI. Обе арх.
 */
#include <stdio.h>
#include <string.h>

#include <linux/device.h>
#include <linux/module.h>

/* ─ фейковая шина: матч по совпадению имён устройства и драйвера ─ */
static int fake_match(struct device *dev, struct device_driver *drv)
{
	return strcmp(dev_name(dev), "fakedev0") == 0 && strcmp(drv->name, "fakedrv") == 0;
}

static struct bus_type fake_bus = {
	.name = "fake",
	.match = fake_match,
};

/* ─ драйвер ─ */
static int probe_called;
static int fake_probe(struct device *dev)
{
	probe_called++;
	printf("  probe: устройство '%s' ← драйвер '%s'\n", dev_name(dev), dev->driver->name);
	dev_set_drvdata(dev, (void *)0xABCD);
	return 0;
}

static int remove_called;
static void *drvdata_at_remove;
static void fake_remove(struct device *dev)
{
	remove_called++;
	drvdata_at_remove = dev_get_drvdata(dev);
	printf("  remove: устройство '%s', drvdata=%p\n", dev_name(dev), drvdata_at_remove);
}

static struct device_driver fake_drv = {
	.name = "fakedrv",
	.bus = &fake_bus,
	.probe = fake_probe,
	.remove = fake_remove,
};

/* ─ устройство ─ */
static struct device fake_dev = {
	.init_name = "fakedev0",
	.bus = &fake_bus,
};

/* ─ «модуль»: module_init перенаправляется в lx_module_init ─ */
static int fake_init(void)
{
	printf("  module_init вызван → driver_register('%s')\n", fake_drv.name);
	return driver_register(&fake_drv);
}
module_init(fake_init);

int main(void)
{
	int ok;

	printf("== Lx_kit driver-model + module_init (Веха 66) ==\n");

	bus_register(&fake_bus);

	/* Устройство есть, драйвера ещё нет — probe НЕ должен вызваться. */
	device_register(&fake_dev);
	printf("после device_register (драйвера нет): probe_called=%d (ждём 0)\n", probe_called);

	/* module_init регистрирует драйвер → матч с устройством → probe. */
	lx_module_init();
	printf("после lx_module_init: probe_called=%d, связан с '%s', drvdata=%p (ждём 0xabcd)\n",
	       probe_called, fake_dev.driver ? fake_dev.driver->name : "нет", dev_get_drvdata(&fake_dev));

	/* Снятие устройства → remove. */
	device_unregister(&fake_dev);
	printf("после device_unregister: remove_called=%d\n", remove_called);

	ok = probe_called == 1 && fake_dev.driver == NULL && remove_called == 1 &&
	     drvdata_at_remove == (void *)0xABCD;
	printf("Результат: probe=%d remove=%d drvdata=%p — %s\n",
	       probe_called, remove_called, drvdata_at_remove, ok ? "OK" : "FAIL");
	return 0;
}
