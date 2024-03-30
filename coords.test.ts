import { commonParent, highestCommonTile } from './coords';

test('commonParent', () => {
	expect(commonParent({ z: 1, x: 1, y: 1 }, { z: 1, x: 1, y: 1})).toEqual({ z: 1, x: 1, y: 1 });
	expect(commonParent({ z: 1, x: 1, y: 0 }, { z: 1, x: 1, y: 1})).toEqual({ z: 0, x: 0, y: 0 });
	expect(commonParent({ z: 2, x: 2, y: 2 }, { z: 1, x: 1, y: 1})).toEqual({ z: 1, x: 1, y: 1 });
	expect(commonParent({ z: 1, x: 1, y: 1 }, { z: 2, x: 2, y: 2})).toEqual({ z: 1, x: 1, y: 1 });
});

test('highestCommonTile', () => {
	const kwLl: [number, number] = [-80.4925, 43.4516];
	const torontoLl: [number, number] = [-79.3832, 43.6532];

	{
		const kw = highestCommonTile([kwLl]);
		expect(kw).toEqual({ z: 18, x: 72459, y: 95873 });
	}

	{
		const toronto = highestCommonTile([torontoLl]);
		expect(toronto).toEqual({ z: 18, x: 73266, y: 95670 });
	}

	{
		const union = highestCommonTile([kwLl, torontoLl]);
		expect(union).toEqual({ z: 7, x: 35, y: 46 });
	}
});
