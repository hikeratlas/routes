import { BboxBitmap } from './bbox-bitmap';

test('test it works', () => {
	const bitmap = BboxBitmap();
	expect(bitmap.test([12, 20])).toBe(false);
	bitmap.add({type: 'Point', coordinates: [12, 20]});
	expect(bitmap.test([12, 20])).toBe(true);
});
