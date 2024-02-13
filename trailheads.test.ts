import type { NodeMap } from './types';
import * as lib from './lib';

const features = lib.load('../basemap/ztrails.geojsonl');
const map: NodeMap = lib.buildIndex(features);

test('trailheads seem reasonable', async () => {
	const ths = lib.getTrailheads(map);

	console.log(ths);

	const names = ths.flatMap(th => map[th].nodes.filter(x => x.name && lib.isFootPath(x)).map(x => x.name));
	names.sort();
	console.log(names);

	// way highway=footway connecting to node amenity=parking
	// https://www.openstreetmap.org/node/302292993
	expect(names.find(x => x === 'Mizzy Lake Trail')).toBe('Mizzy Lake Trail');

	// way highway=path connecting to node amenity=parking
	// https://www.openstreetmap.org/node/3725249095
	expect(names.find(x => x === 'Bat Lake Trail')).toBe('Bat Lake Trail');

	// a way that reaches into an area amenity=parking; the way shares a node
	// with the boundary of the area
	// https://www.openstreetmap.org/way/338153829
	// CONSIDER: should we pick the "best" start/end point? Or just generate all
	//           the paths, and take the longest one?
	expect(names.find(x => x === 'Track and Tower Trail')).toBe('Track and Tower Trail');


	// a way that reaches into an area amenity=parking; the way shares no nodes
	// with the boundary of the area
	// https://www.openstreetmap.org/way/338160813
	expect(names.find(x => x === 'Western Uplands Backpacking Trail')).toBe('Western Uplands Backpacking Trail');

	// parking lot with no name
	// https://www.openstreetmap.org/way/41285694#map=19/45.57910/-78.40638
	expect(names.find(x => x === 'Lookout Trail')).toBe('Lookout Trail');

	// accessed directly from a road
	// https://www.openstreetmap.org/node/1445913060#map=19/45.57947/-78.51268
	expect(names.find(x => x === 'Two Rivers Trail')).toBe('Two Rivers Trail');

	// TODO:
	// Parking lots that are "close" to a trail,
	// - Centennial Ridges Trail (note: there are 2 trailheads, the other one is fine)
	//   https://www.openstreetmap.org/way/298155743
	//
	// - Logging Museum Trail - there's a short (< 20m), unnamed  segment before
	//   this trail starts, on a highway=service
	//   https://www.openstreetmap.org/way/237602381#map=19/45.54394/-78.26273
});
