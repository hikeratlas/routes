import type { Feature, Geometry } from './types';
import { commonParent, highestCommonTile, highestCommonTileForGeometry } from './coords';

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

test('highestCommonTileForGeometry', () => {
	{
		const pt: Geometry = {
			"coordinates": [
				-80.82165546714158,
				43.13660033088897
			],
			"type": "Point"
		};

		const rv = highestCommonTileForGeometry(pt);
		expect(rv).toEqual({ z: 18, x: 72219, y: 96188 });
	}

	{
		// This illustrates a flaw in this approach: this is like a ~20km line,
		// but we have to go up to z6 to get a spanning tile.
		const ls: Geometry = {
			"coordinates": [
				[
					-80.64630208164274,
					43.14755259363292
				],
				[
					-80.58942340103341,
					43.04353688428628
				]
			],
			"type": "LineString"
		};
		const rv = highestCommonTileForGeometry(ls);
		expect(rv).toEqual({ z: 6, x: 17, y: 23});
	}

	{
		const mp: Geometry = {
				"coordinates": [[
					[
						[
							-80.82590798416277,
							43.09375960940383
						],
						[
							-80.68700504442631,
							43.13969698504002
						],
						[
							-80.85774284039184,
							43.174114553944264
						],
						[
							-80.82590798416277,
							43.09375960940383
						]
					]
				]],
				"type": "MultiPolygon"
		};
		const rv = highestCommonTileForGeometry(mp);
		expect(rv).toEqual({z: 11, x: 564, y: 751});
	}
});
