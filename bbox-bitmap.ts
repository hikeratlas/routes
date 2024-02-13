import type { Feature, Geometry } from './types';
import { lon2x, lat2y } from './coords';

// A very naive spatial indexing data structure. You can add some number of features, then
// query to know if a feature definitely does _not_ intersect the added features.
const envelope = require('@turf/envelope').default;

// Not clear how far we can push the zoom level before OOM is a real concern --
// I don't know if Bun eagerly allocates entries, or uses a map-like structure?
const zoom = 17;
const zoomWidth = Math.pow(2, zoom);

export const BboxBitmap = () => {
	//const m: { [k: number]: boolean } = {};
	const m: boolean[] = [];
	let numFeatures = 0;

	return {
		add: (geom: Feature | Geometry) => {
			numFeatures++;
			const box = envelope(geom);

			let minLon = 999, minLat = 999, maxLon = -999, maxLat = -999;

			for (const [lon, lat] of box.geometry.coordinates[0]) {
				if (lon < minLon) minLon = lon;
				if (lat < minLat) minLat = lat;
				if (lon > maxLon) maxLon = lon;
				if (lat > maxLat) maxLat = lat;
			}

			const minX = lon2x(minLon, zoom);
			const minY = lat2y(minLat, zoom);
			const maxX = lon2x(maxLon, zoom);
			const maxY = lat2y(maxLat, zoom);

			for (let x = minX; x <= maxX; x++) {
				for (let y = minY; y <= maxY; y++) {
					m[x * zoomWidth + y] = true;
				}
			}
		},
		test: (coords: [number, number]) => {
			const x = lon2x(coords[0], zoom);
			const y = lat2y(coords[1], zoom);

			if (m[x * zoomWidth + y])
				return true;

			return false;
		}
	};
};
