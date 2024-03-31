import type { Feature } from './types';
import * as lib from './lib';

const SimpleTrail: Feature = {
	type: 'Feature',
	geometry: {
		type: 'LineString',
		coordinates: [
			[1, 1],
			[2, 2],
			[3, 3],
			[1, 1]
		]
	},
	properties: {
	}
};

test('buildIndex', () => {
	// Verify that the map has two-way connectivity for its edges.
	const map = lib.buildIndex([SimpleTrail]);

	// Should be able to traverse the path forwards...
	expect(map['1,1'].edges).toContainEqual([2, 2]);
	expect(map['2,2'].edges).toContainEqual([3, 3]);
	expect(map['3,3'].edges).toContainEqual([1, 1]);

	// And backwards
	expect(map['1,1'].edges).toContainEqual([3, 3]);
	expect(map['2,2'].edges).toContainEqual([1, 1]);
	expect(map['3,3'].edges).toContainEqual([2, 2]);

});
